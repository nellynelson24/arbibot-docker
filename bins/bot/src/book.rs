use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio::time::{interval, sleep, timeout};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use venues::Venue;

const SILENCE: Duration = Duration::from_secs(30);
const KEEPALIVE: Duration = Duration::from_secs(15);
const RECONNECT: Duration = Duration::from_secs(2);
const QUIET_AFTER: Duration = Duration::from_secs(10);
const BYBIT_DEPTH: usize = 50;

#[derive(Clone, Debug, Default)]
pub struct Book {
    pub bids: Vec<(f64, f64)>,
    pub asks: Vec<(f64, f64)>,
    pub updated: Option<Instant>,
    pub bid_moved: Option<Instant>,
    pub ask_moved: Option<Instant>,
}

impl Book {
    pub fn buy_price(&self, amount: f64) -> Option<f64> {
        vwap(&self.asks, amount)
    }

    pub fn sell_price(&self, amount: f64) -> Option<f64> {
        vwap(&self.bids, amount)
    }

    pub fn buy_fill(&self, qty: f64) -> Option<(f64, f64)> {
        fill_qty(&self.asks, qty)
    }

    pub fn sell_fill(&self, qty: f64) -> Option<(f64, f64)> {
        fill_qty(&self.bids, qty)
    }

    pub fn mid(&self) -> Option<f64> {
        Some((self.bids.first()?.0 + self.asks.first()?.0) / 2.0)
    }

    pub fn age(&self) -> Option<Duration> {
        self.updated.map(|t| t.elapsed())
    }
}

fn fill_qty(levels: &[(f64, f64)], qty: f64) -> Option<(f64, f64)> {
    let (mut filled, mut cost) = (0.0, 0.0);
    for &(price, size) in levels {
        let take = (qty - filled).min(size);
        filled += take;
        cost += take * price;
        if filled >= qty * (1.0 - 1e-12) {
            return Some((cost / filled, price));
        }
    }
    None
}

fn vwap(levels: &[(f64, f64)], amount: f64) -> Option<f64> {
    let (mut spent, mut qty) = (0.0, 0.0);
    for &(price, size) in levels {
        let take = (amount - spent).min(price * size);
        spent += take;
        qty += take / price;
        if spent >= amount * (1.0 - 1e-12) {
            return Some(spent / qty);
        }
    }
    None
}

#[derive(Clone, Copy, Debug)]
pub struct Trade {
    pub price: f64,
    pub qty: f64,
}

pub fn spawn(venue: Venue, base: &str, quote: &str) -> watch::Receiver<Book> {
    start(venue, base, quote, None)
}

pub fn spawn_with_trades(
    base: &str,
    quote: &str,
) -> (watch::Receiver<Book>, mpsc::UnboundedReceiver<Trade>) {
    let (trades_tx, trades_rx) = mpsc::unbounded_channel();
    (start(Venue::Okx, base, quote, Some(trades_tx)), trades_rx)
}

fn start(
    venue: Venue,
    base: &str,
    quote: &str,
    trades: Option<mpsc::UnboundedSender<Trade>>,
) -> watch::Receiver<Book> {
    let (tx, rx) = watch::channel(Book::default());
    let (base, quote) = (base.to_owned(), quote.to_owned());
    tokio::spawn(async move {
        let mut failures = 0u32;
        loop {
            let started = Instant::now();
            if let Err(e) = session(venue, &base, &quote, &tx, trades.as_ref()).await {
                if started.elapsed() > QUIET_AFTER {
                    failures = 0;
                }
                failures += 1;

                if failures % 30 == 1 {
                    eprintln!(
                        "{}: książka {base}/{quote} rozłączona ({e:#})",
                        venue.name()
                    );
                }
            }
            if tx.is_closed() {
                return;
            }
            tx.send_replace(Book::default());
            sleep(RECONNECT).await;
        }
    });
    rx
}

async fn session(
    venue: Venue,
    base: &str,
    quote: &str,
    tx: &watch::Sender<Book>,
    trades: Option<&mpsc::UnboundedSender<Trade>>,
) -> Result<()> {
    let (url, subscribe, ping) = match venue {
        Venue::Okx => {
            let id = format!("{base}-{quote}");
            let mut channels = vec![
                json!({"channel": "books5", "instId": id}),
                json!({"channel": "bbo-tbt", "instId": id}),
            ];
            if trades.is_some() {
                channels.push(json!({"channel": "trades", "instId": id}));
            }
            (
                "wss://wseea.okx.com:8443/ws/v5/public",
                json!({"op": "subscribe", "args": channels}),
                "ping".to_owned(),
            )
        }
        Venue::Bybit => (
            "wss://stream.bybit.eu/v5/public/spot",
            json!({"op": "subscribe", "args": [format!("orderbook.{BYBIT_DEPTH}.{base}{quote}")]}),
            json!({"op": "ping"}).to_string(),
        ),
        other => bail!("{}: bot handluje tylko na OKX EEA i Bybit EU", other.name()),
    };
    let (mut ws, _) = connect_async(url).await?;
    ws.send(Message::Text(subscribe.to_string().into())).await?;

    let mut book = Book::default();
    let mut okx_top = OkxTop::default();
    let mut last_trade_id = 0u64;
    let mut last_keepalive_ms = wall_ms();
    let mut keepalive = interval(KEEPALIVE);
    keepalive.tick().await;
    loop {
        tokio::select! {
            _ = keepalive.tick() => {
                let now = wall_ms();
                if now - last_keepalive_ms > 2 * KEEPALIVE.as_millis() {
                    bail!("przerwa w pracy komputera");
                }
                last_keepalive_ms = now;
                if book.age().is_none_or(|age| age > SILENCE) {
                    bail!("brak danych książki przez {} s", SILENCE.as_secs());
                }
                ws.send(Message::Text(ping.clone().into())).await?;
            }
            msg = timeout(SILENCE, ws.next()) => {
                let msg = msg.context("brak wiadomości")?.context("połączenie zamknięte")??;
                let Message::Text(text) = msg else { continue };
                let Ok(v) = serde_json::from_str::<Value>(text.as_str()) else { continue };
                if v["arg"]["channel"] == "trades" {
                    for t in v["data"].as_array().into_iter().flatten() {
                        let id = t["tradeId"].as_str().and_then(|id| id.parse::<u64>().ok());
                        if let Some(id) = id {
                            if id <= last_trade_id {
                                continue;
                            }
                            last_trade_id = id;
                        }
                        if let (Some(trades), Some(price), Some(qty)) = (trades, num(&t["px"]), num(&t["sz"])) {
                            let _ = trades.send(Trade { price, qty });
                        }
                    }
                    continue;
                }
                let top = (best(&book.bids), best(&book.asks));
                let changed = match venue {
                    Venue::Okx => apply_okx(&mut book, &mut okx_top, &v)?,
                    _ => apply_bybit(&mut book, &v)?,
                };
                if changed {
                    let now = Instant::now();
                    book.updated = Some(now);
                    if best(&book.bids) != top.0 {
                        book.bid_moved = Some(now);
                    }
                    if best(&book.asks) != top.1 {
                        book.ask_moved = Some(now);
                    }
                    if tx.send(book.clone()).is_err() {
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn best(levels: &[(f64, f64)]) -> Option<f64> {
    levels.first().map(|l| l.0)
}

#[derive(Default)]
struct OkxTop {
    ts: f64,
    bid: Option<(f64, f64)>,
    ask: Option<(f64, f64)>,
}

fn apply_okx(book: &mut Book, top: &mut OkxTop, v: &Value) -> Result<bool> {
    if v["event"] == "error" {
        bail!("OKX: {}", v["msg"]);
    }
    let Some(data) = v["data"].get(0) else {
        return Ok(false);
    };
    let ts = num(&data["ts"]).unwrap_or(0.0);
    match v["arg"]["channel"].as_str() {
        Some("books5") => {
            book.bids = levels(&data["bids"]);
            book.asks = levels(&data["asks"]);
            if ts >= top.ts {
                *top = OkxTop {
                    ts,
                    bid: book.bids.first().copied(),
                    ask: book.asks.first().copied(),
                };
                return Ok(true);
            }
        }
        Some("bbo-tbt") if ts >= top.ts => {
            *top = OkxTop {
                ts,
                bid: levels(&data["bids"]).first().copied(),
                ask: levels(&data["asks"]).first().copied(),
            };
        }
        _ => return Ok(false),
    }
    if let Some(best) = top.bid {
        book.bids.retain(|&(price, _)| price < best.0);
        book.bids.insert(0, best);
    }
    if let Some(best) = top.ask {
        book.asks.retain(|&(price, _)| price > best.0);
        book.asks.insert(0, best);
    }
    Ok(true)
}

fn apply_bybit(book: &mut Book, v: &Value) -> Result<bool> {
    if v["success"] == false {
        bail!("Bybit: {}", v["ret_msg"]);
    }
    if !v["topic"]
        .as_str()
        .is_some_and(|t| t.starts_with("orderbook."))
    {
        return Ok(false);
    }
    let data = &v["data"];
    match v["type"].as_str() {
        Some("snapshot") => {
            book.bids = levels(&data["b"]);
            book.asks = levels(&data["a"]);
        }
        Some("delta") if book.updated.is_some() => {
            merge(&mut book.bids, &levels_with_zeros(&data["b"]), |a, b| {
                b.total_cmp(a)
            });
            merge(
                &mut book.asks,
                &levels_with_zeros(&data["a"]),
                f64::total_cmp,
            );
        }

        _ => return Ok(false),
    }
    Ok(true)
}

fn merge(
    side: &mut Vec<(f64, f64)>,
    changes: &[(f64, f64)],
    order: fn(&f64, &f64) -> std::cmp::Ordering,
) {
    for &(price, size) in changes {
        let found = side
            .iter()
            .position(|&(p, _)| (p - price).abs() <= price * 1e-12);
        match (found, size > 0.0) {
            (Some(i), true) => side[i].1 = size,
            (Some(i), false) => {
                side.remove(i);
            }
            (None, true) => side.push((price, size)),
            (None, false) => {}
        }
    }
    side.sort_by(|a, b| order(&a.0, &b.0));
    side.truncate(BYBIT_DEPTH);
}

fn levels(v: &Value) -> Vec<(f64, f64)> {
    levels_with_zeros(v)
        .into_iter()
        .filter(|&(_, size)| size > 0.0)
        .collect()
}

fn levels_with_zeros(v: &Value) -> Vec<(f64, f64)> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|level| Some((num(&level[0])?, num(&level[1])?)))
        .collect()
}

fn wall_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

fn num(v: &Value) -> Option<f64> {
    v.as_str()?.parse().ok().filter(|x: &f64| x.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vwap_walks_levels() {
        let asks = vec![(1.0, 10.0), (2.0, 10.0)];

        let price = vwap(&asks, 20.0).unwrap();
        assert!((price - 20.0 / 15.0).abs() < 1e-12);
        assert!(vwap(&asks, 31.0).is_none(), "only 30 quote visible");
    }

    #[test]
    fn fill_by_quantity_reports_average_and_worst_price() {
        let asks = vec![(1.0, 10.0), (2.0, 10.0)];
        let (avg, worst) = fill_qty(&asks, 15.0).unwrap();
        assert!((avg - 20.0 / 15.0).abs() < 1e-12);
        assert_eq!(worst, 2.0);
        assert!(fill_qty(&asks, 21.0).is_none());
    }

    #[test]
    fn okx_best_level_overrides_older_depth_only() {
        let msg = |channel: &str, ts: u64, bid: &str, ask: &str| {
            json!({"arg": {"channel": channel}, "data": [{
                "ts": ts.to_string(),
                "bids": [[bid, "1"], ["0.90", "1"]],
                "asks": [[ask, "1"], ["1.10", "1"]],
            }]})
        };
        let (mut book, mut top) = (Book::default(), OkxTop::default());
        apply_okx(&mut book, &mut top, &msg("books5", 100, "0.99", "1.01")).unwrap();

        apply_okx(&mut book, &mut top, &msg("bbo-tbt", 150, "0.995", "1.05")).unwrap();
        assert_eq!(book.bids, vec![(0.995, 1.0), (0.99, 1.0), (0.90, 1.0)]);
        assert_eq!(book.asks, vec![(1.05, 1.0), (1.10, 1.0)]);

        apply_okx(&mut book, &mut top, &msg("books5", 120, "0.99", "1.01")).unwrap();
        assert_eq!(book.asks, vec![(1.05, 1.0), (1.10, 1.0)]);

        apply_okx(&mut book, &mut top, &msg("books5", 200, "0.98", "1.02")).unwrap();
        assert_eq!(book.bids[0], (0.98, 1.0));

        assert!(!apply_okx(&mut book, &mut top, &msg("bbo-tbt", 180, "0.5", "2")).unwrap());
    }

    #[test]
    fn bybit_delta_updates_removes_and_inserts() {
        let mut bids = vec![(10.0, 1.0), (9.0, 1.0)];
        merge(&mut bids, &[(10.0, 0.0), (9.0, 2.0), (9.5, 3.0)], |a, b| {
            b.total_cmp(a)
        });
        assert_eq!(bids, vec![(9.5, 3.0), (9.0, 2.0)]);
    }
}
