use std::collections::HashMap;

use anyhow::{Context, Result};
use clap::ValueEnum;
use reqwest::Client;
use serde_json::Value;

pub struct Quote {
    pub bid: f64,
    pub bid_qty: Option<f64>,
    pub ask: f64,
    pub ask_qty: Option<f64>,
    pub volume: Option<f64>,
}

pub type Book = HashMap<(String, String), Quote>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, ValueEnum)]
pub enum Venue {
    Okx,
    Bybit,
    Kraken,
    Bitvavo,
    Bitstamp,
}

impl Venue {
    pub const ALL: [Venue; 5] = [
        Venue::Okx,
        Venue::Bybit,
        Venue::Kraken,
        Venue::Bitvavo,
        Venue::Bitstamp,
    ];
    pub const COUNT: usize = Self::ALL.len();

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Okx => "okx",
            Self::Bybit => "bybit",
            Self::Kraken => "kraken",
            Self::Bitvavo => "bitvavo",
            Self::Bitstamp => "bitstamp",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Okx => "OKX EEA",
            Self::Bybit => "Bybit EU",
            Self::Kraken => "Kraken",
            Self::Bitvavo => "Bitvavo",
            Self::Bitstamp => "Bitstamp",
        }
    }

    pub fn taker_fee(self) -> f64 {
        match self {
            Self::Okx => 0.0010,
            Self::Bybit => 0.0025,
            Self::Bitvavo => 0.0025,
            Self::Kraken | Self::Bitstamp => 0.0040,
        }
    }

    pub fn maker_fee(self) -> f64 {
        match self {
            Self::Okx => 0.0008,
            Self::Bybit => 0.0025,
            Self::Bitvavo => 0.0015,
            Self::Kraken => 0.0025,
            Self::Bitstamp => 0.0030,
        }
    }

    pub async fn fetch_quotes(self, client: &Client) -> Result<Book> {
        let book = match self {
            Self::Okx => {
                let v = get_json(
                    client,
                    "https://eea.okx.com/api/v5/market/tickers?instType=SPOT",
                )
                .await?;
                array(&v["data"])?
                    .iter()
                    .filter_map(|t| {
                        let (base, quote) = t["instId"].as_str()?.split_once('-')?;
                        market(
                            base,
                            quote,
                            &t["bidPx"],
                            &t["bidSz"],
                            &t["askPx"],
                            &t["askSz"],
                        )
                        .map(|(k, q)| (k, with_volume(q, &t["volCcy24h"])))
                    })
                    .collect()
            }
            Self::Bybit => {
                let (tickers, instruments) = tokio::try_join!(
                    get_json(
                        client,
                        "https://api.bybit.eu/v5/market/tickers?category=spot"
                    ),
                    get_json(
                        client,
                        "https://api.bybit.eu/v5/market/instruments-info?category=spot"
                    ),
                )?;
                let pairs: HashMap<&str, (&str, &str)> = array(&instruments["result"]["list"])?
                    .iter()
                    .filter_map(|i| {
                        let pair = (i["baseCoin"].as_str()?, i["quoteCoin"].as_str()?);
                        Some((i["symbol"].as_str()?, pair))
                    })
                    .collect();
                array(&tickers["result"]["list"])?
                    .iter()
                    .filter_map(|t| {
                        let (base, quote) = pairs.get(t["symbol"].as_str()?)?;
                        market(
                            base,
                            quote,
                            &t["bid1Price"],
                            &t["bid1Size"],
                            &t["ask1Price"],
                            &t["ask1Size"],
                        )
                        .map(|(k, q)| (k, with_volume(q, &t["turnover24h"])))
                    })
                    .collect()
            }
            Self::Kraken => {
                let (tickers, pairs) = tokio::try_join!(
                    get_json(client, "https://api.kraken.com/0/public/Ticker"),
                    get_json(client, "https://api.kraken.com/0/public/AssetPairs"),
                )?;
                let pairs = &pairs["result"];
                tickers["result"]
                    .as_object()
                    .context("brak listy tickerów Krakena")?
                    .iter()
                    .filter_map(|(key, t)| {
                        let (base, quote) = pairs[key]["wsname"].as_str()?.split_once('/')?;

                        market(
                            kraken_asset(base),
                            kraken_asset(quote),
                            &t["b"][0],
                            &t["b"][2],
                            &t["a"][0],
                            &t["a"][2],
                        )
                    })
                    .collect()
            }
            Self::Bitvavo => {
                let v = get_json(client, "https://api.bitvavo.com/v2/ticker/book").await?;
                array(&v)?
                    .iter()
                    .filter_map(|t| {
                        let (base, quote) = t["market"].as_str()?.split_once('-')?;
                        market(
                            base,
                            quote,
                            &t["bid"],
                            &t["bidSize"],
                            &t["ask"],
                            &t["askSize"],
                        )
                    })
                    .collect()
            }
            Self::Bitstamp => {
                let v = get_json(client, "https://www.bitstamp.net/api/v2/ticker/").await?;
                array(&v)?
                    .iter()
                    .filter_map(|t| {
                        let (base, quote) = t["pair"].as_str()?.split_once('/')?;

                        market(
                            base,
                            quote,
                            &t["bid"],
                            &Value::Null,
                            &t["ask"],
                            &Value::Null,
                        )
                    })
                    .collect()
            }
        };
        Ok(book)
    }
}

async fn get_json(client: &Client, url: &str) -> Result<Value> {
    let body = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    serde_json::from_str(&body).with_context(|| format!("niepoprawny JSON z {url}"))
}

fn array(v: &Value) -> Result<&Vec<Value>> {
    v.as_array().context("brak listy rynków w odpowiedzi")
}

fn market(
    base: &str,
    quote: &str,
    bid: &Value,
    bid_qty: &Value,
    ask: &Value,
    ask_qty: &Value,
) -> Option<((String, String), Quote)> {
    let key = (base.to_uppercase(), quote.to_uppercase());
    Some((
        key,
        Quote {
            bid: num(bid)?,
            bid_qty: num(bid_qty),
            ask: num(ask)?,
            ask_qty: num(ask_qty),
            volume: None,
        },
    ))
}

fn with_volume(quote: Quote, volume: &Value) -> Quote {
    Quote {
        volume: num(volume),
        ..quote
    }
}

fn kraken_asset(code: &str) -> &str {
    match code {
        "XBT" => "BTC",
        "XDG" => "DOGE",
        other => other,
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
