use std::collections::HashMap;
use std::time::{Duration, Instant};

use account::{Credentials, Okx, OrderEvent, OrderUpdate, Scope};
use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};
use venues::Venue;

use super::maker::{Resting, index, reserve};
use super::orders::text;
use super::{BYBIT, Engine, OKX, clock};
use crate::inventory::Wallet;
use crate::ledger::now_ms;
use crate::paper::Side;

const DEAD_MAN_SECS: u32 = 30;
const DEAD_MAN_EVERY: Duration = Duration::from_secs(10);
const RECONNECT: Duration = Duration::from_secs(2);
const MAX_UNHEDGED: Duration = Duration::from_secs(60);
const MAX_REJECTS: u32 = 5;
const ID_PREFIX: &str = "fbm";
const NO_ORDER: &str = "51603";

enum Command {
    Place {
        id: String,
        side: Side,
        qty: String,
        price: String,
    },
    Cancel(String),
    DeadMan,
}

pub(super) enum Event {
    Order(OrderEvent),
    Rejected { id: String, error: String },
    CancelFailed { id: String, error: String },
    Disconnected(String),
}

struct Own {
    side: Side,
    qty: f64,
    price: f64,
    booked: f64,
    booked_value: f64,
}

pub(super) struct LiveMaker {
    commands: mpsc::UnboundedSender<Command>,
    orders: HashMap<String, Own>,
    pub(super) connected: bool,
    rejects: u32,
    last_dead_man: Instant,
    seq: u32,
    unhedged_since: [Option<Instant>; 2],
}

impl LiveMaker {
    fn place(&mut self, side: Side, quote: &Resting, tick: f64, step: f64) -> String {
        self.seq += 1;
        let id = format!("{ID_PREFIX}{}x{}", now_ms() as u64, self.seq);
        self.orders.insert(
            id.clone(),
            Own {
                side,
                qty: quote.qty,
                price: quote.price,
                booked: 0.0,
                booked_value: 0.0,
            },
        );
        let _ = self.commands.send(Command::Place {
            id: id.clone(),
            side,
            qty: text(quote.qty, step),
            price: text(quote.price, tick),
        });
        id
    }

    fn cancel(&self, id: &str) {
        let _ = self.commands.send(Command::Cancel(id.to_owned()));
    }
}

impl Engine {
    pub(super) async fn start_live_maker(&mut self) -> Result<()> {
        let (Some(trade), Some(maker)) = (&self.live, &mut self.maker) else {
            return Ok(());
        };
        let (base, quote) = (self.market.base.clone(), self.market.quote.clone());
        for id in trade.okx.open_orders(&base, &quote).await? {
            if id.starts_with(ID_PREFIX) {
                trade.okx.cancel(&base, &quote, &id).await?;
                println!("anulowano pozostałe zlecenie maker {id}");
            }
        }
        trade
            .okx
            .cancel_all_after(DEAD_MAN_SECS)
            .await
            .context("OKX: automatyczne anulowanie zleceń (dead man's switch) niedostępne")?;

        let (commands, mut rx) = mpsc::unbounded_channel();
        let (events, events_rx) = mpsc::unbounded_channel();
        let orders = Okx::new(
            self.http.clone(),
            Credentials::load(Venue::Okx, Scope::Trade)?,
        );
        let stream = Okx::new(
            self.http.clone(),
            Credentials::load(Venue::Okx, Scope::Read)?,
        );
        let tx = events.clone();
        let (b, q) = (base.clone(), quote.clone());
        tokio::spawn(async move {
            while let Some(command) = rx.recv().await {
                match command {
                    Command::Place {
                        id,
                        side,
                        qty,
                        price,
                    } => {
                        if let Err(e) = orders
                            .place_post_only(&b, &q, side, &qty, &price, &id)
                            .await
                        {
                            let _ = tx.send(Event::Rejected {
                                id,
                                error: format!("{e:#}"),
                            });
                        }
                    }
                    Command::Cancel(id) => {
                        let mut result = Ok(());
                        for _ in 0..3 {
                            result = orders.cancel(&b, &q, &id).await;
                            if result.is_ok() {
                                break;
                            }
                            sleep(Duration::from_millis(200)).await;
                        }
                        if let Err(e) = result {
                            let _ = tx.send(Event::CancelFailed {
                                id,
                                error: format!("{e:#}"),
                            });
                        }
                    }
                    Command::DeadMan => {
                        if let Err(e) = orders.cancel_all_after(DEAD_MAN_SECS).await {
                            println!("[{}] OKX dead man's switch: {e:#}", clock());
                        }
                    }
                }
            }
        });
        tokio::spawn(async move {
            loop {
                let ended = stream
                    .order_updates(&base, &quote, |e| {
                        let _ = events.send(Event::Order(e));
                    })
                    .await;
                let reason = ended
                    .err()
                    .map_or("koniec".to_owned(), |e| format!("{e:#}"));
                if events.send(Event::Disconnected(reason)).is_err() {
                    return;
                }
                sleep(RECONNECT).await;
            }
        });
        maker.live = Some(LiveMaker {
            commands,
            orders: HashMap::new(),
            connected: false,
            rejects: 0,
            last_dead_man: Instant::now(),
            seq: 0,
            unhedged_since: [None, None],
        });
        self.maker_events = Some(events_rx);
        Ok(())
    }

    pub(super) fn send_quotes(&mut self, old: [Option<String>; 2], placed: [bool; 2]) {
        let (tick, step) = (self.market.tick[OKX], self.market.step);
        let Some(maker) = &mut self.maker else {
            return;
        };
        let Some(live) = &mut maker.live else {
            return;
        };
        for (i, side) in [Side::Buy, Side::Sell].into_iter().enumerate() {
            let quote = &mut maker.quotes[i];
            if let Some(id) = &old[i]
                && (placed[i] || quote.as_ref().and_then(|q| q.id.as_ref()) != Some(id))
            {
                live.cancel(id);
            }
            if placed[i]
                && let Some(quote) = quote
            {
                quote.id = Some(live.place(side, quote, tick, step));
            }
        }
    }

    pub(super) fn cancel_quotes(&mut self) {
        let Some(maker) = &mut self.maker else {
            return;
        };
        for quote in &mut maker.quotes {
            if let (Some(live), Some(id)) =
                (&maker.live, quote.as_ref().and_then(|q| q.id.as_ref()))
            {
                live.cancel(id);
            }
            *quote = None;
        }
    }

    pub(super) fn free(&self) -> [Wallet; 2] {
        let Some(maker) = &self.maker else {
            return self.inventory.wallets;
        };
        let orders: Vec<(Side, f64, f64)> = maker
            .live
            .iter()
            .flat_map(|l| l.orders.values())
            .map(|own| (own.side, (own.qty - own.booked).max(0.0), own.price))
            .collect();
        reserve(
            self.inventory.wallets,
            &orders,
            (maker.unhedged, maker.unhedged_cash),
            [self.market.fees[OKX].maker, self.market.fees[BYBIT].taker],
        )
    }

    pub(super) async fn on_maker_event(&mut self, event: Event) -> Result<()> {
        let Some(live) = self.maker.as_mut().and_then(|m| m.live.as_mut()) else {
            return Ok(());
        };
        match event {
            Event::Order(OrderEvent::Subscribed) => {
                live.connected = true;
                println!("[{}] maker: strumień zleceń OKX połączony", clock());

                let ids: Vec<String> = live.orders.keys().cloned().collect();
                for id in ids {
                    self.check_order(&id).await?;
                }
            }
            Event::Order(OrderEvent::Update(update)) => self.on_order(update).await?,
            Event::Rejected { id, error } => {
                live.rejects += 1;
                let rejects = live.rejects;
                if let Some(maker) = &mut self.maker {
                    for quote in &mut maker.quotes {
                        if quote.as_ref().and_then(|q| q.id.as_deref()) == Some(id.as_str()) {
                            *quote = None;
                        }
                    }
                }
                println!("[{}] maker: zlecenie odrzucone ({error})", clock());

                self.check_order(&id).await?;
                if rejects >= MAX_REJECTS && self.halted.is_none() {
                    self.halt(format!(
                        "{rejects} odrzuconych zleceń maker z rzędu: {error}"
                    ))?;
                }
            }
            Event::CancelFailed { id, error } => {
                if self.halted.is_none() {
                    self.halt(format!(
                        "nie da się anulować zlecenia maker {id} ({error}) — sprawdź zlecenia na OKX"
                    ))?;
                }
            }
            Event::Disconnected(reason) => {
                if live.connected {
                    live.connected = false;
                    println!(
                        "[{}] maker: strumień zleceń OKX rozłączony ({reason})",
                        clock()
                    );
                }
                self.cancel_quotes();
            }
        }
        Ok(())
    }

    async fn check_order(&mut self, id: &str) -> Result<()> {
        let (base, quote) = (&self.market.base, &self.market.quote);
        match self.accounts.okx.order_state(base, quote, id).await {
            Ok(update) => self.on_order(update).await?,
            Err(e) if format!("{e}").contains(NO_ORDER) => {
                if let Some(live) = self.maker.as_mut().and_then(|m| m.live.as_mut()) {
                    live.orders.remove(id);
                }
            }
            Err(e) => println!("[{}] maker: stan zlecenia {id}: {e:#}", clock()),
        }
        Ok(())
    }

    async fn on_order(&mut self, update: OrderUpdate) -> Result<()> {
        let Some(maker) = &mut self.maker else {
            return Ok(());
        };
        let Some(live) = &mut maker.live else {
            return Ok(());
        };
        let Some(own) = live.orders.get_mut(&update.client_id) else {
            return Ok(());
        };
        live.rejects = 0;
        let side = own.side;

        let (mut qty, mut cash) = (0.0, 0.0);
        if update.filled > own.booked {
            let value = update.filled * update.avg_price;
            (qty, cash) = (update.filled - own.booked, value - own.booked_value);
            own.booked = update.filled;
            own.booked_value = value;
        }
        if update.done {
            live.orders.remove(&update.client_id);
        }
        let i = index(side);
        if let Some(quote) = &mut maker.quotes[i]
            && quote.id.as_deref() == Some(update.client_id.as_str())
        {
            quote.qty -= qty;
            if update.done {
                maker.quotes[i] = None;
            }
        }
        if qty > 0.0 {
            let id = serde_json::json!(update.client_id);
            self.book_maker_fill(side, qty, cash, ("id", id))?;
            self.hedge(side).await?;
        }
        Ok(())
    }

    pub(super) fn maker_tick(&mut self) -> Result<()> {
        let mid = self.mid();
        let min_notional = self.market.min_notional;
        let Some(maker) = &mut self.maker else {
            return Ok(());
        };
        let Some(live) = &mut maker.live else {
            return Ok(());
        };

        if self.halted.is_none() && live.last_dead_man.elapsed() >= DEAD_MAN_EVERY {
            let _ = live.commands.send(Command::DeadMan);
            live.last_dead_man = Instant::now();
        }
        let mut stuck = None;
        for i in 0..2 {
            let hedgeable = mid.is_some_and(|mid| maker.unhedged[i] * mid >= min_notional.max(1.0));
            let since = &mut live.unhedged_since[i];
            match (hedgeable, *since) {
                (false, _) => *since = None,
                (true, None) => *since = Some(Instant::now()),
                (true, Some(t)) if t.elapsed() > MAX_UNHEDGED => stuck = Some(maker.unhedged[i]),
                _ => {}
            }
        }
        if let Some(qty) = stuck
            && self.halted.is_none()
        {
            self.halt(format!(
                "{qty:.4} {} z makera bez zabezpieczenia od ponad {} s — sprawdź salda",
                self.market.base,
                MAX_UNHEDGED.as_secs()
            ))?;
        }
        Ok(())
    }

    pub(super) async fn stop_live_maker(&mut self) {
        let (Some(trade), Some(maker)) = (&self.live, &self.maker) else {
            return;
        };
        let Some(live) = &maker.live else {
            return;
        };
        let (base, quote) = (&self.market.base, &self.market.quote);
        let mut all_cancelled = true;
        for id in live.orders.keys() {
            let cancel = timeout(Duration::from_secs(5), trade.okx.cancel(base, quote, id));
            match cancel.await {
                Ok(Ok(())) => println!("anulowano zlecenie maker {id}"),
                Ok(Err(e)) => {
                    all_cancelled = false;
                    println!("anulowanie zlecenia maker {id}: {e:#}");
                }
                Err(_) => {
                    all_cancelled = false;
                    println!("anulowanie zlecenia maker {id}: brak odpowiedzi");
                }
            }
        }

        if all_cancelled && let Err(e) = trade.okx.cancel_all_after(0).await {
            println!("OKX dead man's switch: {e:#}");
        }
    }
}
