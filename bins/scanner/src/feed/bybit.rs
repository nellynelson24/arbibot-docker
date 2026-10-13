use anyhow::Result;
use serde_json::{Value, json};

use super::{Raw, RawTop, RawTrade, first_level, num};

pub const URL: &str = "wss://stream.bybit.eu/v5/public/spot";
pub const PING: &str = r#"{"op":"ping"}"#;

pub fn subscribe(symbols: &[&str]) -> Vec<String> {
    let topics: Vec<String> = symbols
        .iter()
        .flat_map(|s| [format!("orderbook.1.{s}"), format!("publicTrade.{s}")])
        .collect();
    topics
        .chunks(10)
        .map(|chunk| json!({"op": "subscribe", "args": chunk}).to_string())
        .collect()
}

pub fn parse(v: &Value, out: &mut Vec<Raw>) -> Result<()> {
    if v["success"] == false {
        eprintln!("Bybit EU: {}", v["ret_msg"]);
        return Ok(());
    }
    let Some(topic) = v["topic"].as_str() else {
        return Ok(());
    };
    if topic.starts_with("orderbook.") {
        let Some(symbol) = v["data"]["s"].as_str() else {
            return Ok(());
        };

        let snapshot = v["type"] == "snapshot";
        let side = |levels: &Value| match levels.as_array() {
            Some(l) if l.is_empty() && !snapshot => (None, None),
            _ => first_level(levels),
        };
        let (bid, bid_qty) = side(&v["data"]["b"]);
        let (ask, ask_qty) = side(&v["data"]["a"]);
        out.push(Raw::Top(RawTop {
            symbol: symbol.to_owned(),
            bid,
            bid_qty,
            ask,
            ask_qty,
        }));
    } else if topic.starts_with("publicTrade.") {
        for t in v["data"].as_array().into_iter().flatten() {
            if let (Some(symbol), Some(price), Some(qty)) =
                (t["s"].as_str(), num(&t["p"]), num(&t["v"]))
            {
                out.push(Raw::Trade(RawTrade {
                    symbol: symbol.to_owned(),
                    price,
                    qty,
                    taker_buys: t["S"] == "Buy",
                }));
            }
        }
    }
    Ok(())
}
