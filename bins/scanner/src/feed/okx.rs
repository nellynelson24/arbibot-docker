use anyhow::Result;
use serde_json::{Value, json};

use super::{Raw, RawTop, RawTrade, first_level, num};

pub const URL: &str = "wss://wseea.okx.com:8443/ws/v5/public";
pub const PING: &str = "ping";

pub fn subscribe(symbols: &[&str]) -> Vec<String> {
    let args: Vec<Value> = symbols
        .iter()
        .flat_map(|s| {
            [
                json!({"channel": "bbo-tbt", "instId": s}),
                json!({"channel": "trades", "instId": s}),
            ]
        })
        .collect();
    args.chunks(100)
        .map(|chunk| json!({"op": "subscribe", "args": chunk}).to_string())
        .collect()
}

pub fn parse(v: &Value, out: &mut Vec<Raw>) -> Result<()> {
    if v["event"] == "error" {
        eprintln!("OKX EEA: {}", v["msg"]);
        return Ok(());
    }
    let (Some(symbol), Some(data)) = (v["arg"]["instId"].as_str(), v["data"].as_array()) else {
        return Ok(());
    };
    match v["arg"]["channel"].as_str() {
        Some("bbo-tbt") => {
            for book in data {
                let (bid, bid_qty) = first_level(&book["bids"]);
                let (ask, ask_qty) = first_level(&book["asks"]);
                out.push(Raw::Top(RawTop {
                    symbol: symbol.to_owned(),
                    bid,
                    bid_qty,
                    ask,
                    ask_qty,
                }));
            }
        }
        Some("trades") => {
            for t in data {
                if let (Some(price), Some(qty)) = (num(&t["px"]), num(&t["sz"])) {
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
