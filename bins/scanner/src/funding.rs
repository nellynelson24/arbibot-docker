use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::{Map, Value, json};
use tokio::time::interval;

use crate::clock::now_ms;
use crate::engine::JsonlLog;

const URL: &str = "https://futures.kraken.com/derivatives/api/v3/tickers";
const EVERY: Duration = Duration::from_secs(300);
const HOURS_PER_YEAR: f64 = 24.0 * 365.0;

pub async fn run(client: Client, data_dir: PathBuf) {
    let mut log = match JsonlLog::new(data_dir.join("funding")) {
        Ok(log) => log,
        Err(e) => {
            eprintln!("Kraken Futures: brak zapisu stóp finansowania ({e:#})");
            return;
        }
    };
    let mut every = interval(EVERY);
    loop {
        every.tick().await;
        let result = match fetch(&client).await {
            Ok(rates) => {
                let now = now_ms();
                let line = json!({
                    "type": "funding",
                    "ts_ms": now as u64,
                    "interval_secs": EVERY.as_secs(),
                    "apr": rates,
                });
                log.write(now, &line)
            }
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            eprintln!("Kraken Futures: stopy finansowania ({e:#})");
        }
    }
}

async fn fetch(client: &Client) -> Result<Map<String, Value>> {
    let body = client
        .get(URL)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let v: Value = serde_json::from_str(&body)?;
    let mut rates = Map::new();
    for t in v["tickers"].as_array().context("brak listy kontraktów")? {
        let (Some(symbol), Some(hourly)) =
            (t["symbol"].as_str(), t["relativeFundingRate"].as_f64())
        else {
            continue;
        };
        let Some(asset) = symbol
            .strip_prefix("PF_")
            .and_then(|s| s.strip_suffix("USD"))
        else {
            continue;
        };
        let asset = match asset {
            "XBT" => "BTC",
            "XDG" => "DOGE",
            other => other,
        };
        rates.insert(asset.to_owned(), json!(hourly * HOURS_PER_YEAR));
    }
    Ok(rates)
}
