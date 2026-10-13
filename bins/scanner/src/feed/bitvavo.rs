use anyhow::Result;
use serde_json::{Value, json};

use super::{Raw, RawTop, RawTrade, num};

pub const URL: &str = "wss://ws.bitvavo.com/v2/";

pub fn subscribe(symbols: &[&str]) -> Vec<String> {
    symbols
        .chunks(100)
        .map(|chunk| {
            json!({
                "action": "subscribe",
                "channels": [
                    {"name": "ticker", "markets": chunk},
                    {"name": "trades", "markets": chunk}
                ]
            })
            .to_string()
        })
        .collect()
}

pub fn parse(v: &Value, out: &mut Vec<Raw>) -> Result<()> {
    if v.get("errorCode").is_some() {
        eprintln!("Bitvavo: {}", v["error"]);
        return Ok(());
    }
    let Some(symbol) = v["market"].as_str() else {
        return Ok(());
    };
    match v["event"].as_str() {
        Some("ticker") => out.push(Raw::Top(RawTop {
            symbol: symbol.to_owned(),
            bid: num(&v["bestBid"]),
            bid_qty: num(&v["bestBidSize"]),
            ask: num(&v["bestAsk"]),
            ask_qty: num(&v["bestAskSize"]),
        })),
        Some("trade") => {
            if let (Some(price), Some(qty)) = (num(&v["price"]), num(&v["amount"])) {
                out.push(Raw::Trade(RawTrade {
                    symbol: symbol.to_owned(),
                    price,
                    qty,
                    taker_buys: v["side"] == "buy",
                }));
            }
        }
        _ => {}
    }
    Ok(())
}
