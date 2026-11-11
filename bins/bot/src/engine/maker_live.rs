use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use account::{Credentials, Okx, OrderEvent, OrderUpdate, Scope};
use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};
use venues::Venue;

use serde_json::{Value, json};

use super::maker::{Resting, below_minimum, index, reserve};
use super::orders::{side_id, text};
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
pub(super) const SWEEP_EVERY: Duration = Duration::from_secs(30);
const ORPHAN_WINDOW: Duration = Duration::from_secs(60);
const DRAIN_ON_STOP: Duration = Duration::from_secs(3);

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
    Rejected {
        id: String,
        error: String,
        definite: bool,
    },
    CancelFailed {
        id: String,
        error: String,
    },
    Disconnected(String),
}

struct Own {
    side: Side,
    qty: f64,
    price: f64,
    booked: f64,
    booked_value: f64,
    placed_at: Instant,
}

pub(super) struct LiveMaker {
    commands: mpsc::UnboundedSender<Command>,
    orders: HashMap<String, Own>,
    pub(super) connected: bool,
    rejects: u32,
    last_dead_man: Instant,
    seq: u32,
    unhedged_since: [Option<Instant>; 2],
    unhedged_alerted: bool,
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
                placed_at: Instant::now(),
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
        let mut leftover_ids = Vec::new();
        for id in trade.okx.open_orders(&base, &quote).await? {
            if id.starts_with(ID_PREFIX) {
                trade.okx.cancel(&base, &quote, &id).await?;
                println!("anulowano pozostałe zlecenie maker {id}");
                leftover_ids.push(id);
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
                                definite: account::rejected(&e),
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
            unhedged_alerted: false,
        });
        self.maker_events = Some(events_rx);
        self.reconcile_leftovers(&leftover_ids).await
    }

    async fn reconcile_leftovers(&mut self, leftover_ids: &[String]) -> Result<()> {
        let Some(trade) = &self.live else {
            return Ok(());
        };
        let (base, quote) = (&self.market.base, &self.market.quote);
        let mut orders = Vec::new();
        for id in leftover_ids {
            match trade.okx.order_state(base, quote, id).await {
                Ok(update) => orders.push((update, now_ms())),
                Err(e) => println!("maker: stan pozostałego zlecenia {id}: {e:#}"),
            }
        }
        match trade.okx.recent_orders(base, quote).await {
            Ok(recent) => orders.extend(recent),
            Err(e) => println!("maker: historia zleceń OKX niedostępna ({e:#})"),
        }
        let found = leftovers(&self.ledger.history(), &orders, self.market.fees[OKX].maker);
        let m = &self.market;
        let (step, limits) = (m.step, (m.min_qty, m.min_notional));
        let base = m.base.clone();
        let quote = m.quote.clone();
        for (i, side) in [Side::Buy, Side::Sell].into_iter().enumerate() {
            let (qty, cash) = found[i];
            if qty <= 0.0 || below_minimum(qty, cash.abs() / qty, step, limits) {
                continue;
            }
            let adopt = self.hedge_leftovers;
            self.ledger.write(json!({
                "type": "leftover",
                "side": side_id(side),
                "qty": qty,
                "cash": cash,
                "adopted": adopt,
            }))?;
            let line = format!(
                "Po poprzedniej sesji zostało bez zabezpieczenia: {} {qty:.4} {base} na {} po średnio {:.6} {quote}. {}",
                match side {
                    Side::Buy => "kupno",
                    Side::Sell => "sprzedaż",
                },
                super::VENUES[OKX].name(),
                cash.abs() / qty,
                if adopt {
                    "Bot zabezpieczy to na Bybit EU."
                } else {
                    "Bot tego nie zabezpiecza. Jeśli nie zrobiłeś tego ręcznie, zabezpiecz na Bybit EU albo uruchom bota z --hedge-leftovers."
                }
            );
            println!("[{}] {line}", clock());
            self.tell(line);
            if adopt && let Some(maker) = &mut self.maker {
                maker.unhedged[i] += qty;
                maker.unhedged_cash[i] += cash;
                maker.filled_at[i].get_or_insert_with(Instant::now);
            }
        }
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

    pub(super) fn replaced_orders(&self) -> Vec<(Side, f64, f64)> {
        let Some(maker) = &self.maker else {
            return Vec::new();
        };
        let current: Vec<&str> = maker
            .quotes
            .iter()
            .flatten()
            .filter_map(|q| q.id.as_deref())
            .collect();
        maker
            .live
            .iter()
            .flat_map(|l| l.orders.iter())
            .filter(|(id, _)| !current.contains(&id.as_str()))
            .map(|(_, own)| (own.side, (own.qty - own.booked).max(0.0), own.price))
            .collect()
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
            Event::Rejected {
                id,
                error,
                definite,
            } => {
                live.rejects += 1;
                let rejects = live.rejects;
                if definite {
                    live.orders.remove(&id);
                }
                if let Some(maker) = &mut self.maker {
                    for quote in &mut maker.quotes {
                        if quote.as_ref().and_then(|q| q.id.as_deref()) == Some(id.as_str()) {
                            *quote = None;
                        }
                    }
                }
                println!("[{}] maker: zlecenie odrzucone ({error})", clock());

                if !definite {
                    self.check_order(&id).await?;
                }
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
            Ok(update) => {
                let open = !update.done;
                self.on_order(update).await?;
                if open
                    && !self.current_quote_ids().contains(&id.to_owned())
                    && let Some(live) = self.maker.as_ref().and_then(|m| m.live.as_ref())
                {
                    println!("[{}] maker: anuluję zlecenie {id} bez kwotowania", clock());
                    live.cancel(id);
                }
            }
            Err(e) if format!("{e}").contains(NO_ORDER) => {
                if let Some(live) = self.maker.as_mut().and_then(|m| m.live.as_mut())
                    && live
                        .orders
                        .get(id)
                        .is_some_and(|o| o.placed_at.elapsed() >= ORPHAN_WINDOW)
                {
                    live.orders.remove(id);
                }
            }
            Err(e) => println!("[{}] maker: stan zlecenia {id}: {e:#}", clock()),
        }
        Ok(())
    }

    fn current_quote_ids(&self) -> Vec<String> {
        self.maker
            .iter()
            .flat_map(|m| m.quotes.iter().flatten())
            .filter_map(|q| q.id.clone())
            .collect()
    }

    pub(super) async fn sweep_orders(&mut self) -> Result<()> {
        let current = self.current_quote_ids();
        let Some(live) = self.maker.as_ref().and_then(|m| m.live.as_ref()) else {
            return Ok(());
        };
        let stale = stale_orders(&live.orders, &current, Instant::now());
        for id in stale {
            self.check_order(&id).await?;
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
        let halted = self.halted.is_some();
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
        let alert = stuck.is_some() && halted && !live.unhedged_alerted;
        live.unhedged_alerted = stuck.is_some() && (live.unhedged_alerted || halted);
        if let Some(qty) = stuck {
            if !halted {
                self.halt(format!(
                    "{qty:.4} {} z makera bez zabezpieczenia od ponad {} s — sprawdź salda",
                    self.market.base,
                    MAX_UNHEDGED.as_secs()
                ))?;
            } else if alert {
                self.tell(format!(
                    "Bot zatrzymany, a {qty:.4} {} z makera jest bez zabezpieczenia od ponad {} s. {}",
                    self.market.base,
                    MAX_UNHEDGED.as_secs(),
                    if self.hedge_while_paused {
                        "Bot ponawia zabezpieczenie co 10 s."
                    } else {
                        "Zabezpiecz ręcznie na Bybit EU albo uruchom bota z --hedge-while-paused."
                    }
                ));
            }
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
        self.drain_maker_events().await;
    }

    async fn drain_maker_events(&mut self) {
        let Some(mut events) = self.maker_events.take() else {
            return;
        };
        let deadline = Instant::now() + DRAIN_ON_STOP;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match timeout(left, events.recv()).await {
                Ok(Some(Event::Order(OrderEvent::Update(update)))) => {
                    if let Err(e) = self.on_order(update).await {
                        println!("[{}] maker przy zamykaniu: {e:#}", clock());
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
    }
}

fn stale_orders(orders: &HashMap<String, Own>, current: &[String], now: Instant) -> Vec<String> {
    orders
        .iter()
        .filter(|(id, own)| {
            !current.contains(id) && now.saturating_duration_since(own.placed_at) >= SWEEP_EVERY
        })
        .map(|(id, _)| id.clone())
        .collect()
}

fn leftovers(rows: &[Value], orders: &[(OrderUpdate, f64)], maker_fee: f64) -> [(f64, f64); 2] {
    let Some(start) = rows.iter().rposition(|r| r["type"] == "start") else {
        return [(0.0, 0.0); 2];
    };
    let since = rows[start]["ts_ms"].as_f64().unwrap_or(0.0);
    let cash = |i: usize, qty: f64, price: f64| match i {
        0 => -qty * price * (1.0 + maker_fee),
        _ => qty * price * (1.0 - maker_fee),
    };
    let side = |v: &Value| match v.as_str() {
        Some("buy") => Some(0),
        Some("sell") => Some(1),
        _ => None,
    };
    let num = |r: &Value, key: &str| r[key].as_f64().unwrap_or(0.0);
    let mut out = [(0.0, 0.0); 2];
    for r in &rows[start + 1..] {
        let Some(i) = side(&r["side"]) else {
            continue;
        };
        match r["type"].as_str() {
            Some("maker_fill") => {
                out[i].0 += num(r, "qty");
                out[i].1 += cash(i, num(r, "qty"), num(r, "price"));
            }
            Some("leftover") if r["adopted"] == true => {
                out[i].0 += num(r, "qty");
                out[i].1 += num(r, "cash");
            }
            Some("maker_hedge") if out[i].0 > 0.0 => {
                let share = (num(r, "qty") / out[i].0).min(1.0);
                out[i].1 -= out[i].1 * share;
                out[i].0 -= num(r, "qty");
            }
            _ => {}
        }
    }
    for o in &mut out {
        if o.0 <= 0.0 {
            *o = (0.0, 0.0);
        }
    }
    let mut booked: HashMap<&str, f64> = HashMap::new();
    for r in rows.iter().filter(|r| r["type"] == "maker_fill") {
        if let Some(id) = r["id"].as_str() {
            *booked.entry(id).or_default() += num(r, "qty");
        }
    }
    let mut seen = HashSet::new();
    for (update, created) in orders {
        let id = update.client_id.as_str();
        if !id.starts_with(ID_PREFIX) || *created < since || !seen.insert(id) {
            continue;
        }
        let extra = update.filled - booked.get(id).copied().unwrap_or(0.0);
        if extra > 1e-9 {
            let i = index(update.side);
            out[i].0 += extra;
            out[i].1 += cash(i, extra, update.avg_price);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn own(age: Duration) -> Own {
        Own {
            side: Side::Buy,
            qty: 100.0,
            price: 0.18,
            booked: 0.0,
            booked_value: 0.0,
            placed_at: Instant::now() - age,
        }
    }

    #[test]
    fn orders_without_a_quote_are_checked_after_a_while() {
        let mut orders = HashMap::new();
        orders.insert("fbm1".to_owned(), own(Duration::from_secs(45)));
        orders.insert("fbm2".to_owned(), own(Duration::from_secs(45)));
        orders.insert("fbm3".to_owned(), own(Duration::from_secs(5)));
        let stale = stale_orders(&orders, &["fbm2".to_owned()], Instant::now());
        assert_eq!(stale, ["fbm1"]);
    }

    fn update(id: &str, side: Side, filled: f64, price: f64) -> OrderUpdate {
        OrderUpdate {
            client_id: id.to_owned(),
            side,
            filled,
            avg_price: price,
            done: true,
        }
    }

    #[test]
    fn leftovers_come_from_the_last_session_and_unbooked_fills() {
        let rows = vec![
            json!({"type": "start", "ts_ms": 1_000.0}),
            json!({"type": "maker_fill", "side": "buy", "qty": 97.2, "price": 0.2, "id": "fbm1"}),
            json!({"type": "start", "ts_ms": 2_000.0}),
            json!({"type": "maker_fill", "side": "buy", "qty": 100.0, "price": 0.18, "id": "fbm2"}),
            json!({"type": "maker_hedge", "side": "buy", "qty": 100.0}),
            json!({"type": "maker_fill", "side": "sell", "qty": 50.0, "price": 0.19, "id": "fbm3"}),
        ];
        let orders = vec![
            (update("fbm3", Side::Sell, 80.0, 0.19), 2_500.0),
            (update("fbm3", Side::Sell, 80.0, 0.19), 2_500.0),
            (update("fbm1", Side::Buy, 150.0, 0.2), 1_500.0),
            (update("other", Side::Buy, 10.0, 0.2), 2_500.0),
        ];
        let found = leftovers(&rows, &orders, 0.0);
        assert_eq!(found[0], (0.0, 0.0));
        assert!((found[1].0 - 80.0).abs() < 1e-9);
        assert!((found[1].1 - 80.0 * 0.19).abs() < 1e-9);
        assert_eq!(leftovers(&rows[..2], &[], 0.0)[0].0, 97.2);
        assert_eq!(leftovers(&[], &orders, 0.0), [(0.0, 0.0); 2]);
    }
}
