mod args;
mod live;
mod maker;
mod maker_live;
mod market;
mod orders;
mod rebalance;
mod status;
mod switch;
mod taker;

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::AtomicU32;
use std::time::{Duration, Instant};

use account::{Bybit, Credentials, Fees, Okx, Scope};
use anyhow::{Context, Result, bail};
use chrono::DateTime;
use serde_json::json;
use tokio::sync::{mpsc, watch};
use tokio::time::{interval, interval_at, sleep};
use venues::Venue;

use crate::book::{self, Book, Trade};
use crate::inventory::Inventory;
use crate::ledger::{Ledger, now_ms};
use crate::paper::Side;
use crate::selector;
use crate::telegram::{Notifier, Telegram};

pub use args::{Args, LiveArgs};
pub use live::live;
use maker::Maker;
use market::load_market;

const VENUES: [Venue; 2] = [Venue::Okx, Venue::Bybit];
const OKX: usize = 0;
const BYBIT: usize = 1;
const MAX_BOOK_AGE: Duration = Duration::from_secs(30);
const STATUS_EVERY: Duration = Duration::from_secs(60);
const STOP_FILE: &str = "STOP";
const MAX_ROUTE_COST: f64 = 0.01;
const REMAINDER_SLIPPAGE: f64 = 0.01;
const INVENTORY_SLIPPAGE: f64 = 0.003;
const SIZE_MARGIN: f64 = 0.01;

struct Accounts {
    okx: Okx,
    bybit: Bybit,
}

struct Market {
    base: String,
    quote: String,
    fees: [Fees; 2],
    tick: [f64; 2],
    step: f64,
    min_qty: f64,
    min_notional: f64,
    base_fee: [Option<f64>; 2],
    quote_fee: [Option<f64>; 2],
}

struct Plan {
    qty: f64,
    buy_limit: f64,
    sell_limit: f64,
    expected_bps: f64,
}

#[derive(Default)]
struct Stats {
    signals: u32,
    no_funds: u32,
    trades: u32,
    matched: u32,
    with_remainder: u32,
    unfilled: u32,
    pnl: f64,
    rebalances: u32,
    rebalance_cost: f64,
    switches: u32,
    switch_cost: f64,
    rebalance_trades: u32,
    rebalance_trade_pnl: f64,
    maker_fills: u32,
    maker_hedges: u32,
    maker_pnl: f64,
    hedge_misses: u32,
}

struct Engine {
    market: Market,
    accounts: Accounts,
    live: Option<Accounts>,
    order_seq: AtomicU32,
    orders: Mutex<Vec<serde_json::Value>>,
    slack_bps: f64,
    trade_rebalance: bool,
    rebalance_max_cost_bps: f64,
    last_trade_rebalance: Option<Instant>,
    http: reqwest::Client,
    data_dir: PathBuf,
    route_limit: f64,
    switching: bool,
    pending_switch: Option<(String, u32)>,
    telegram: Option<Notifier>,
    last_blocked: String,
    day_trades: u32,
    maker: Option<Maker>,
    trades: Option<mpsc::UnboundedReceiver<Trade>>,
    maker_events: Option<mpsc::UnboundedReceiver<maker_live::Event>>,
    books: [watch::Receiver<Book>; 2],
    latency: [Duration; 2],
    size: f64,
    min_bps: f64,
    daily_loss_limit: f64,
    transfer: Duration,
    inventory: Inventory,
    ledger: Ledger,
    stop_file: PathBuf,
    armed: [bool; 2],
    paused_until: Option<Instant>,
    halted: Option<String>,
    last_tick_ms: f64,
    last_report: Option<String>,
    day: String,
    day_pnl: f64,
    start_equity: f64,
    started: Instant,
    stats: Stats,
}

pub async fn dry_run(args: Args) -> Result<()> {
    run(args, None).await
}

fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?)
}

async fn run(args: Args, live: Option<(Accounts, bool, bool)>) -> Result<()> {
    let http = http()?;
    let accounts = Accounts {
        okx: Okx::new(http.clone(), Credentials::load(Venue::Okx, Scope::Read)?),
        bybit: Bybit::new(http.clone(), Credentials::load(Venue::Bybit, Scope::Read)?),
    };
    let market = load_market(&args, &http, &accounts).await?;
    let (live, buy_inventory, confirmed) = match live {
        Some((trade, buy, yes)) => (Some(trade), buy, yes),
        None => (None, false, true),
    };
    let (books, trades) = open_books(&market, args.maker && live.is_none());
    let mid = wait_for_books(&books).await?;
    let inventory = Inventory::new(args.capital, mid);
    let (label, journal) = match live {
        Some(_) => ("NA ŻYWO", "live"),
        None => ("na sucho", "bot"),
    };
    let telegram = match args.telegram {
        true => Some(Telegram::load()?.start(label, args.data_dir.join(STOP_FILE))),
        false => None,
    };
    let mut engine = Engine {
        start_equity: inventory.equity(mid),
        inventory,
        accounts,
        live,
        order_seq: AtomicU32::new(0),
        orders: Mutex::new(Vec::new()),
        slack_bps: args.slack_bps,
        trade_rebalance: args.trade_rebalance,
        rebalance_max_cost_bps: args.rebalance_max_cost_bps,
        last_trade_rebalance: None,
        http,
        data_dir: args.data_dir.clone(),
        route_limit: MAX_ROUTE_COST * args.capital,
        switching: args.switch,
        pending_switch: None,
        telegram,
        last_blocked: String::new(),
        day_trades: 0,
        maker: args.maker.then(|| {
            let size = args.maker_size.unwrap_or(args.size);
            Maker::new(size, args.maker_min_bps, args.requote_ms)
        }),
        trades,
        maker_events: None,
        books,
        latency: [
            Duration::from_millis(args.okx_latency_ms),
            Duration::from_millis(args.bybit_latency_ms),
        ],
        size: args.size,
        min_bps: args.min_bps,
        daily_loss_limit: args.daily_loss_limit,
        transfer: Duration::from_secs(args.transfer_mins * 60),
        ledger: Ledger::new(args.data_dir.join(journal))?,
        stop_file: args.data_dir.join(STOP_FILE),
        armed: [true; 2],
        paused_until: None,
        halted: None,
        last_tick_ms: now_ms(),
        last_report: None,
        day: selector::utc_date(now_ms()),
        day_pnl: 0.0,
        started: Instant::now(),
        stats: Stats::default(),
        market,
    };

    let result = engine.work(&args, mid, buy_inventory, confirmed).await;
    engine.stop_live_maker().await;
    if let Err(e) = &result {
        engine.tell(format!("BŁĄD, bot kończy pracę: {e:#}"));
    }
    let summary = engine.summary();
    if let Some(telegram) = engine.telegram.take() {
        telegram.close().await;
    }
    result.and(summary)
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut term) => tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        },
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

impl Engine {
    async fn work(
        &mut self,
        args: &Args,
        mid: f64,
        buy_inventory: bool,
        confirmed: bool,
    ) -> Result<()> {
        if self.live.is_some() {
            self.prepare_live(mid, buy_inventory, confirmed).await?;
            self.start_live_maker().await?;
            self.start_equity = self.inventory.equity(mid);

            self.last_tick_ms = now_ms();
        }
        self.restore_day();
        let (mode, label) = match self.live {
            Some(_) => ("live", "NA ŻYWO"),
            None => ("dry-run", "na sucho"),
        };
        self.ledger.write(json!({
            "type": "start",
            "mode": mode,
            "wallets": self.inventory.wallets.map(|w| [w.base, w.quote]),
            "pair": format!("{}/{}", self.market.base, self.market.quote),
            "capital": args.capital,
            "size": args.size,
            "min_bps": args.min_bps,
            "slack_bps": args.slack_bps,
            "trade_rebalance": args.trade_rebalance,
            "maker": args.maker,
            "maker_min_bps": args.maker_min_bps,
            "latency_ms": [args.okx_latency_ms, args.bybit_latency_ms],
            "fees_taker": [self.market.fees[0].taker, self.market.fees[1].taker],
        }))?;
        let mut started = format!(
            "Start {label}: {}/{q}, kapitał {:.2} {q}, transakcja {} {q}, próg {} bp, \
         dzienny limit straty {} {q}.",
            self.market.base,
            self.start_equity,
            args.size,
            args.min_bps,
            args.daily_loss_limit,
            q = self.market.quote
        );
        if self.day_trades > 0 {
            started += &format!(
                " Dziś wcześniej: transakcje {}, wynik {:+.4} {}.",
                self.day_trades, self.day_pnl, self.market.quote
            );
        }
        println!(
            "{started} Zatrzymanie: Ctrl+C lub plik {}.\n",
            self.stop_file.display()
        );
        self.tell(started);
        self.check_loss_limit()?;

        let mut tick = interval(Duration::from_secs(1));
        let mut status = interval(STATUS_EVERY);
        status.tick().await;
        let review_every = Duration::from_secs_f64(args.review_hours.max(0.01) * 3600.0);
        let mut review = interval_at(tokio::time::Instant::now() + review_every, review_every);
        let shutdown = shutdown_signal();
        tokio::pin!(shutdown);
        'run: loop {
            let [mut okx_changes, mut bybit_changes] = self.books.clone();
            let mut trades = self.trades.take();
            let mut maker_events = self.maker_events.take();
            loop {
                tokio::select! {
                    _ = &mut shutdown => break 'run,
                    Some(trade) = next(&mut trades) => self.on_trade(trade).await?,
                Some(event) = next(&mut maker_events) => self.on_maker_event(event).await?,
                    changed = okx_changes.changed() => {
                        changed.context("feed OKX zakończony")?;
                        self.evaluate().await?;
                    }
                    changed = bybit_changes.changed() => {
                        changed.context("feed Bybit zakończony")?;
                        self.evaluate().await?;
                    }
                    _ = tick.tick() => self.tick()?,
                    _ = status.tick() => self.status()?,
                    _ = review.tick(), if self.switching => match self.review().await {
                        Ok(true) => continue 'run,
                        Ok(false) => {}
                        Err(e) => println!("[{}] ocena par nieudana: {e:#}", clock()),
                    },
                }
            }
        }
        Ok(())
    }
}

impl Engine {
    async fn evaluate(&mut self) -> Result<()> {
        if self.halted.is_some() || self.paused_until.is_some_and(|t| Instant::now() < t) {
            self.cancel_quotes();
            return Ok(());
        }
        if self.maker.is_some() {
            for side in [Side::Buy, Side::Sell] {
                self.hedge(side).await?;
            }
        }
        self.taker().await?;
        if self.trade_rebalance {
            self.rebalance_by_trade().await?;
        }
        self.refresh_quotes();
        Ok(())
    }

    fn mid(&self) -> Option<f64> {
        let (sum, n) = self
            .books
            .iter()
            .filter_map(|b| b.borrow().mid())
            .fold((0.0, 0), |(sum, n), mid| (sum + mid, n + 1));
        (n > 0).then(|| sum / f64::from(n))
    }

    fn tell(&self, text: impl Into<String>) {
        if let Some(telegram) = &self.telegram {
            telegram.send(text);
        }
    }
}

async fn wait_for_books(books: &[watch::Receiver<Book>; 2]) -> Result<f64> {
    for _ in 0..60 {
        let mids: Vec<f64> = books.iter().filter_map(|b| b.borrow().mid()).collect();
        if mids.len() == 2 {
            return Ok(mids.iter().sum::<f64>() / 2.0);
        }
        sleep(Duration::from_secs(1)).await;
    }
    bail!("brak książek obu giełd po 60 s")
}

fn open_books(
    market: &Market,
    with_trades: bool,
) -> (
    [watch::Receiver<Book>; 2],
    Option<mpsc::UnboundedReceiver<Trade>>,
) {
    let bybit = book::spawn(Venue::Bybit, &market.base, &market.quote);
    if with_trades {
        let (okx, trades) = book::spawn_with_trades(&market.base, &market.quote);
        ([okx, bybit], Some(trades))
    } else {
        (
            [book::spawn(Venue::Okx, &market.base, &market.quote), bybit],
            None,
        )
    }
}

async fn next<T>(rx: &mut Option<mpsc::UnboundedReceiver<T>>) -> Option<T> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

fn clock() -> String {
    DateTime::from_timestamp_millis(now_ms() as i64)
        .map(|t| t.format("%H:%M:%S UTC").to_string())
        .unwrap_or_default()
}
