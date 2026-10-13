use anyhow::Result;
use serde_json::{Value, json};

use super::{Raw, RawTop, RawTrade, num};

pub const URL: &str = "wss://ws.kraken.com/v2";
pub const PING: &str = r#"{"method":"ping"}"#;

pub fn subscribe(symbols: &[&str]) -> Vec<String> {
    symbols
        .chunks(50)
        .flat_map(|chunk| {
            [
                json!({
                    "method": "subscribe",
                    "params": {"channel": "ticker", "symbol": chunk, "event_trigger": "bbo"}
                }),
                json!({
                    "method": "subscribe",
                    "params": {"channel": "trade", "symbol": chunk, "snapshot": false}
                }),
            ]
        })
        .map(|msg| msg.to_string())
        .collect()
}

pub fn parse(v: &Value, out: &mut Vec<Raw>) -> Result<()> {
    if v["success"] == false {
        eprintln!("Kraken: {}", v["error"]);
        return Ok(());
    }
    let data = v["data"].as_array().into_iter().flatten();
    match v["channel"].as_str() {
        Some("ticker") => {
            for t in data {
                let Some(symbol) = t["symbol"].as_str() else {
                    continue;
                };
                out.push(Raw::Top(RawTop {
                    symbol: symbol.to_owned(),
                    bid: num(&t["bid"]),
                    bid_qty: num(&t["bid_qty"]),
                    ask: num(&t["ask"]),
                    ask_qty: num(&t["ask_qty"]),
                }));
            }
        }
        Some("trade") => {
            for t in data {
                if let (Some(symbol), Some(price), Some(qty)) =
                    (t["symbol"].as_str(), num(&t["price"]), num(&t["qty"]))
                {
                    out.push(Raw::Trade(RawTrade {
                        symbol: symbol.to_owned(),
                        price,
                        qty,
                        taker_buys: t["side"] == "buy",
                    }));
                }
            }
        }
        _ => {}
    }
    Ok(())
}
