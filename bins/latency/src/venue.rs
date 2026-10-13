use anyhow::{Context, Result, bail};
use chrono::DateTime;
use clap::ValueEnum;
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Venue {
    #[value(help = "OKX, podmiot EEA (Malta)")]
    Okx,
    #[value(help = "Bybit EU (Austria)")]
    Bybit,
    #[value(help = "Kraken (Irlandia)")]
    Kraken,
    #[value(help = "Bitvavo (Holandia)")]
    Bitvavo,
    #[value(help = "Bitstamp (Luksemburg)")]
    Bitstamp,
    #[value(help = "Coinbase Advanced Trade (Luksemburg)")]
    Coinbase,
}

pub enum Ping {
    Text(String),
    Frame,
}

pub enum Feed {
    Book(Option<f64>),
    Other,
}

impl Venue {
    pub const ALL: [Venue; 6] = [
        Venue::Okx,
        Venue::Bybit,
        Venue::Kraken,
        Venue::Bitvavo,
        Venue::Bitstamp,
        Venue::Coinbase,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Okx => "OKX EEA",
            Self::Bybit => "Bybit EU",
            Self::Kraken => "Kraken",
            Self::Bitvavo => "Bitvavo",
            Self::Bitstamp => "Bitstamp",
            Self::Coinbase => "Coinbase",
        }
    }

    pub fn rest_probe_url(self) -> &'static str {
        match self {
            Self::Okx => "https://eea.okx.com/api/v5/public/time",
            Self::Bybit => "https://api.bybit.eu/v5/market/time",
            Self::Kraken => "https://api.kraken.com/0/public/Time",
            Self::Bitvavo => "https://api.bitvavo.com/v2/time",
            Self::Bitstamp => "https://www.bitstamp.net/api/v2/ticker/btcusd/",
            Self::Coinbase => "https://api.coinbase.com/api/v3/brokerage/time",
        }
    }

    pub fn ws_public_url(self) -> &'static str {
        match self {
            Self::Okx => "wss://wseea.okx.com:8443/ws/v5/public",
            Self::Bybit => "wss://stream.bybit.eu/v5/public/spot",
            Self::Kraken => "wss://ws.kraken.com/v2",
            Self::Bitvavo => "wss://ws.bitvavo.com/v2/",
            Self::Bitstamp => "wss://ws.bitstamp.net",
            Self::Coinbase => "wss://advanced-trade-ws.coinbase.com",
        }
    }

    pub fn symbol(self, base: &str, quote: &str) -> String {
        let (base, quote) = (base.to_uppercase(), quote.to_uppercase());
        match self {
            Self::Okx | Self::Bitvavo | Self::Coinbase => format!("{base}-{quote}"),
            Self::Bybit => format!("{base}{quote}"),
            Self::Kraken => format!("{base}/{quote}"),
            Self::Bitstamp => format!("{base}{quote}").to_lowercase(),
        }
    }

    pub fn ping(self, id: u32) -> Ping {
        let msg = match self {
            Self::Okx => return Ping::Text("ping".to_owned()),
            Self::Coinbase => return Ping::Frame,
            Self::Bybit => json!({"op": "ping", "req_id": id.to_string()}),
            Self::Kraken => json!({"method": "ping", "req_id": id}),
            Self::Bitvavo => json!({"action": "getTime"}),
            Self::Bitstamp => json!({"event": "bts:heartbeat"}),
        };
        Ping::Text(msg.to_string())
    }

    pub fn is_pong(self, text: &str) -> bool {
        if let Self::Okx = self {
            return text == "pong";
        }
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            return false;
        };
        match self {
            Self::Bybit => v["ret_msg"] == "pong" || v["op"] == "pong",
            Self::Kraken => v["method"] == "pong",
            Self::Bitvavo => v["action"] == "getTime",
            Self::Bitstamp => v["event"] == "bts:heartbeat",
            Self::Okx | Self::Coinbase => false,
        }
    }

    pub fn subscribe_msg(self, symbol: &str) -> String {
        let msg = match self {
            Self::Okx => {
                json!({"op": "subscribe", "args": [{"channel": "bbo-tbt", "instId": symbol}]})
            }
            Self::Bybit => json!({"op": "subscribe", "args": [format!("orderbook.1.{symbol}")]}),
            Self::Kraken => json!({
                "method": "subscribe",
                "params": {"channel": "book", "symbol": [symbol], "depth": 10}
            }),
            Self::Bitvavo => json!({
                "action": "subscribe",
                "channels": [{"name": "book", "markets": [symbol]}]
            }),
            Self::Bitstamp => json!({
                "event": "bts:subscribe",
                "data": {"channel": format!("order_book_{symbol}")}
            }),
            Self::Coinbase => {
                json!({"type": "subscribe", "product_ids": [symbol], "channel": "level2"})
            }
        };
        msg.to_string()
    }

    pub fn parse_server_time_ms(self, body: &str) -> Result<Option<f64>> {
        let v: Value = serde_json::from_str(body)?;
        let ms = match self {
            Self::Okx => v["data"][0]["ts"].as_str().and_then(|s| s.parse().ok()),
            Self::Bybit => v["result"]["timeNano"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .map(|ns| ns as f64 / 1e6),
            Self::Bitvavo => v["time"].as_f64(),
            Self::Coinbase => v["epochMillis"].as_str().and_then(|s| s.parse().ok()),
            Self::Kraken | Self::Bitstamp => return Ok(None),
        };
        ms.map(Some)
            .with_context(|| format!("nieoczekiwana odpowiedź czasu serwera: {body}"))
    }

    pub fn parse_feed(self, text: &str) -> Result<Feed> {
        if self.is_pong(text) {
            return Ok(Feed::Other);
        }
        let v: Value = serde_json::from_str(text)
            .with_context(|| format!("nieoczekiwana wiadomość: {text}"))?;
        let feed = match self {
            Self::Okx => {
                if v["event"] == "error" {
                    bail!("subskrypcja odrzucona: {}", v["msg"]);
                }
                book_if(v["data"].is_array(), || {
                    v["data"][0]["ts"].as_str().and_then(|s| s.parse().ok())
                })
            }
            Self::Bybit => {
                if v["success"] == false {
                    bail!("subskrypcja odrzucona: {}", v["ret_msg"]);
                }

                book_if(v.get("topic").is_some(), || v["ts"].as_f64())
            }
            Self::Kraken => {
                if v["success"] == false {
                    bail!("subskrypcja odrzucona: {}", v["error"]);
                }
                book_if(v["channel"] == "book" && v["type"] == "update", || {
                    rfc3339_ms(&v["data"][0]["timestamp"])
                })
            }
            Self::Bitvavo => {
                if v.get("errorCode").is_some() {
                    bail!("subskrypcja odrzucona: {}", v["error"]);
                }

                book_if(v["event"] == "book", || {
                    v["timestamp"].as_u64().map(|ns| ns as f64 / 1e6)
                })
            }
            Self::Bitstamp => {
                if v["event"] == "bts:error" {
                    bail!("subskrypcja odrzucona: {}", v["data"]["message"]);
                }
                book_if(v["event"] == "data", || {
                    v["data"]["microtimestamp"]
                        .as_str()
                        .and_then(|s| s.parse::<f64>().ok())
                        .map(|us| us / 1000.0)
                })
            }
            Self::Coinbase => {
                if v["type"] == "error" {
                    bail!("subskrypcja odrzucona: {}", v["message"]);
                }
                book_if(
                    v["channel"] == "l2_data" && v["events"][0]["type"] == "update",
                    || rfc3339_ms(&v["timestamp"]),
                )
            }
        };
        Ok(feed)
    }
}

fn book_if(is_book: bool, timestamp: impl FnOnce() -> Option<f64>) -> Feed {
    if is_book {
        Feed::Book(timestamp())
    } else {
        Feed::Other
    }
}

fn rfc3339_ms(v: &Value) -> Option<f64> {
    let time = DateTime::parse_from_rfc3339(v.as_str()?).ok()?;
    Some(time.timestamp_micros() as f64 / 1000.0)
}
