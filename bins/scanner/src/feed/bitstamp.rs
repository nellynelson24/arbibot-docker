use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::{Raw, RawTop, RawTrade, first_level, num};

pub const URL: &str = "wss://ws.bitstamp.net";
pub const PING: &str = r#"{"event":"bts:heartbeat"}"#;
const BOOK_PREFIX: &str = "order_book_";
const TRADES_PREFIX: &str = "live_trades_";

pub fn subscribe(symbols: &[&str]) -> Vec<String> {
    symbols
        .iter()
        .flat_map(|s| [format!("{BOOK_PREFIX}{s}"), format!("{TRADES_PREFIX}{s}")])
        .map(|channel| json!({"event": "bts:subscribe", "data": {"channel": channel}}).to_string())
        .collect()
}

pub fn parse(v: &Value, out: &mut Vec<Raw>) -> Result<()> {
    let channel = v["channel"].as_str().unwrap_or_default();
    match v["event"].as_str() {
        Some("bts:error") => eprintln!("Bitstamp: {}", v["data"]["message"]),
        Some("bts:request_reconnect") => bail!("giełda poprosiła o ponowne połączenie"),
        Some("data") => {
            let Some(symbol) = channel.strip_prefix(BOOK_PREFIX) else {
                return Ok(());
            };
            let (bid, bid_qty) = first_level(&v["data"]["bids"]);
            let (ask, ask_qty) = first_level(&v["data"]["asks"]);
            out.push(Raw::Top(RawTop {
                symbol: symbol.to_owned(),
                bid,
                bid_qty,
                ask,
                ask_qty,
            }));
        }
        Some("trade") => {
            let Some(symbol) = channel.strip_prefix(TRADES_PREFIX) else {
                return Ok(());
            };
            if let (Some(price), Some(qty)) = (num(&v["data"]["price"]), num(&v["data"]["amount"]))
            {
                out.push(Raw::Trade(RawTrade {
                    symbol: symbol.to_owned(),
                    price,
                    qty,

                    taker_buys: v["data"]["type"] == 0,
                }));
            }
        }
        _ => {}
    }
    Ok(())
}
