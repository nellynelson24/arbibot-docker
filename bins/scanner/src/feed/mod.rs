mod bitstamp;
mod bitvavo;
mod bybit;
mod kraken;
mod okx;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use serde_json::Value;
use tokio::sync::mpsc::Sender;
use tokio::time::{interval, sleep, timeout};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use venues::Venue;

use crate::clock::now_ms;
use crate::universe::{Listing, PairId};

const KEEPALIVE: Duration = Duration::from_secs(15);
const SILENCE_LIMIT: Duration = Duration::from_secs(60);
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const STABLE_SESSION: Duration = Duration::from_secs(60);
const MARKET_SILENCE: Duration = Duration::from_secs(30);
const MARKET_SILENCE_FEW_PAIRS: Duration = Duration::from_secs(600);
const FEW_PAIRS: usize = 20;
const SUBSCRIBE_BURST: usize = 10;
const SUBSCRIBE_PAUSE: Duration = Duration::from_millis(50);
const SEED_ATTEMPTS: u32 = 3;
const SEED_RETRY: Duration = Duration::from_millis(500);

pub enum Event {
    Up(Venue),
    Down(Venue),
    Top(TopUpdate),
    Trade(TradeUpdate),
}

pub struct TopUpdate {
    pub venue: Venue,
    pub pair: PairId,
    pub bid: Option<f64>,
    pub bid_qty: Option<f64>,
    pub ask: Option<f64>,
    pub ask_qty: Option<f64>,
}

#[derive(Clone, Copy)]
pub struct TradeUpdate {
    pub venue: Venue,
    pub pair: PairId,
    pub price: f64,
    pub qty: f64,
    pub taker_buys: bool,
}

pub enum Raw {
    Top(RawTop),
    Trade(RawTrade),
}

pub struct RawTrade {
    pub symbol: String,
    pub price: f64,
    pub qty: f64,
    pub taker_buys: bool,
}

pub struct RawTop {
    pub symbol: String,
    pub bid: Option<f64>,
    pub bid_qty: Option<f64>,
    pub ask: Option<f64>,
    pub ask_qty: Option<f64>,
}

pub fn ws_symbol(venue: Venue, base: &str, quote: &str) -> String {
    match venue {
        Venue::Okx | Venue::Bitvavo => format!("{base}-{quote}"),
        Venue::Bybit => format!("{base}{quote}"),
        Venue::Kraken => format!("{base}/{quote}"),
        Venue::Bitstamp => format!("{base}{quote}").to_lowercase(),
    }
}

pub async fn run(venue: Venue, listings: Vec<Listing>, client: Client, tx: Sender<Event>) {
    let by_symbol: HashMap<&str, PairId> = listings
        .iter()
        .map(|l| (l.symbol.as_str(), l.pair))
        .collect();
    let mut backoff = MIN_BACKOFF;
    loop {
        let started = Instant::now();
        let result = session(venue, &listings, &by_symbol, &client, &tx).await;
        if tx.send(Event::Down(venue)).await.is_err() {
            return;
        }
        if let Err(e) = result {
            eprintln!("{}: rozłączono ({e:#})", venue.name());
        }
        if started.elapsed() > STABLE_SESSION {
            backoff = MIN_BACKOFF;
        }
        sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

async fn session(
    venue: Venue,
    listings: &[Listing],
    by_symbol: &HashMap<&str, PairId>,
    client: &Client,
    tx: &Sender<Event>,
) -> Result<()> {
    let (mut ws, _) = connect_async(url(venue)).await?;
    let symbols: Vec<&str> = listings.iter().map(|l| l.symbol.as_str()).collect();
    for (i, msg) in subscribe_msgs(venue, &symbols).into_iter().enumerate() {
        if i > 0 && i % SUBSCRIBE_BURST == 0 {
            sleep(SUBSCRIBE_PAUSE).await;
        }
        ws.send(Message::Text(msg.into())).await?;
    }

    let seed = fetch_seed(venue, client).await?;
    for listing in listings {
        if let Some(q) = seed.get(&listing.key) {
            let update = TopUpdate {
                venue,
                pair: listing.pair,
                bid: Some(q.bid),
                bid_qty: Some(q.bid_qty.unwrap_or(f64::NAN)),
                ask: Some(q.ask),
                ask_qty: Some(q.ask_qty.unwrap_or(f64::NAN)),
            };
            tx.send(Event::Top(update)).await?;
        }
    }
    tx.send(Event::Up(venue)).await?;

    let mut raw = Vec::new();
    let market_silence = if listings.len() > FEW_PAIRS {
        MARKET_SILENCE
    } else {
        MARKET_SILENCE_FEW_PAIRS
    };
    let mut last_market_data = Instant::now();
    let mut last_keepalive_ms = now_ms();
    let mut keepalive = interval(KEEPALIVE);
    keepalive.tick().await;
    loop {
        tokio::select! {
            _ = keepalive.tick() => {
                let now = now_ms();
                if now - last_keepalive_ms > 2.0 * KEEPALIVE.as_secs_f64() * 1000.0 {
                    bail!("przerwa w pracy komputera");
                }
                last_keepalive_ms = now;
                if last_market_data.elapsed() > market_silence {
                    bail!("brak danych rynkowych przez {} s", market_silence.as_secs());
                }
                if let Some(ping) = ping_msg(venue) {
                    ws.send(Message::Text(ping.into())).await?;
                }
            }
            msg = timeout(SILENCE_LIMIT, ws.next()) => {
                let msg = msg
                    .context("brak danych przez 60 s")?
                    .context("połączenie zamknięte")??;
                let text = match msg {
                    Message::Text(text) => text,
                    Message::Close(frame) => bail!("giełda zamknęła połączenie: {frame:?}"),
                    _ => continue,
                };
                parse(venue, text.as_str(), &mut raw)?;
                if !raw.is_empty() {
                    last_market_data = Instant::now();
                }
                for item in raw.drain(..) {
                    let event = match item {
                        Raw::Top(top) => {
                            let Some(&pair) = by_symbol.get(top.symbol.as_str()) else { continue };
                            Event::Top(TopUpdate {
                                venue,
                                pair,
                                bid: top.bid,
                                bid_qty: top.bid_qty,
                                ask: top.ask,
                                ask_qty: top.ask_qty,
                            })
                        }
                        Raw::Trade(trade) => {
                            let Some(&pair) = by_symbol.get(trade.symbol.as_str()) else { continue };
                            Event::Trade(TradeUpdate {
                                venue,
                                pair,
                                price: trade.price,
                                qty: trade.qty,
                                taker_buys: trade.taker_buys,
                            })
                        }
                    };
                    tx.send(event).await?;
                }
            }
        }
    }
}

async fn fetch_seed(venue: Venue, client: &Client) -> Result<venues::Book> {
    let mut attempt = 1;
    loop {
        match venue.fetch_quotes(client).await {
            Ok(book) => return Ok(book),
            Err(e) if attempt >= SEED_ATTEMPTS => return Err(e).context("migawka REST"),
            Err(_) => {
                attempt += 1;
                sleep(SEED_RETRY).await;
            }
        }
    }
}

fn url(venue: Venue) -> &'static str {
    match venue {
        Venue::Okx => okx::URL,
        Venue::Bybit => bybit::URL,
        Venue::Kraken => kraken::URL,
        Venue::Bitvavo => bitvavo::URL,
        Venue::Bitstamp => bitstamp::URL,
    }
}

fn subscribe_msgs(venue: Venue, symbols: &[&str]) -> Vec<String> {
    match venue {
        Venue::Okx => okx::subscribe(symbols),
        Venue::Bybit => bybit::subscribe(symbols),
        Venue::Kraken => kraken::subscribe(symbols),
        Venue::Bitvavo => bitvavo::subscribe(symbols),
        Venue::Bitstamp => bitstamp::subscribe(symbols),
    }
}

fn ping_msg(venue: Venue) -> Option<&'static str> {
    match venue {
        Venue::Okx => Some(okx::PING),
        Venue::Bybit => Some(bybit::PING),
        Venue::Kraken => Some(kraken::PING),
        Venue::Bitvavo => None,
        Venue::Bitstamp => Some(bitstamp::PING),
    }
}

fn parse(venue: Venue, text: &str, out: &mut Vec<Raw>) -> Result<()> {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return Ok(());
    };
    match venue {
        Venue::Okx => okx::parse(&v, out),
        Venue::Bybit => bybit::parse(&v, out),
        Venue::Kraken => kraken::parse(&v, out),
        Venue::Bitvavo => bitvavo::parse(&v, out),
        Venue::Bitstamp => bitstamp::parse(&v, out),
    }
}

fn first_level(levels: &Value) -> (Option<f64>, Option<f64>) {
    match levels.as_array() {
        Some(l) if l.is_empty() => (Some(f64::NAN), Some(0.0)),
        Some(l) => (num(&l[0][0]), num(&l[0][1])),
        None => (None, None),
    }
}

fn num(v: &Value) -> Option<f64> {
    let x = match v {
        Value::String(s) => s.parse().ok()?,
        Value::Number(n) => n.as_f64()?,
        _ => return None,
    };
    x.is_finite().then_some(x)
}
