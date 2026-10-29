use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use serde_json::{Value, json};
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::{
    Balance, Credentials, Deposit, DepositAddress, Fees, KeyRights, Network, OrderFill, Rejected,
    Rules, Side, num, retry_rate_limited, sign,
};

const BASE: &str = "https://eea.okx.com";
const WS_PRIVATE: &str = "wss://wseea.okx.com:8443/ws/v5/private";
const WS_PING: Duration = Duration::from_secs(20);
const ALREADY_FINAL: [&str; 3] = ["51400", "51401", "51402"];

#[derive(Debug, Clone)]
pub struct OrderUpdate {
    pub client_id: String,
    pub side: Side,
    pub filled: f64,
    pub avg_price: f64,
    pub done: bool,
}

#[derive(Debug, Clone)]
pub enum OrderEvent {
    Subscribed,
    Update(OrderUpdate),
}

pub struct Okx {
    http: Client,
    creds: Credentials,
}

impl Okx {
    pub const TIME_URL: &str = "https://eea.okx.com/api/v5/public/time";

    pub fn new(http: Client, creds: Credentials) -> Self {
        Okx { http, creds }
    }

    pub async fn balances(&self) -> Result<Vec<Balance>> {
        let trading = self.get("/api/v5/account/balance").await?;
        let funding = self.get("/api/v5/asset/balances").await?;
        let mut out = Vec::new();
        for d in trading[0]["details"].as_array().into_iter().flatten() {
            out.push(Balance {
                account: "trading",
                asset: d["ccy"].as_str().unwrap_or_default().to_owned(),
                total: num(&d["cashBal"]).unwrap_or(0.0),
                available: num(&d["availBal"]).unwrap_or(0.0),
            });
        }
        for d in funding.as_array().into_iter().flatten() {
            out.push(Balance {
                account: "funding",
                asset: d["ccy"].as_str().unwrap_or_default().to_owned(),
                total: num(&d["bal"]).unwrap_or(0.0),
                available: num(&d["availBal"]).unwrap_or(0.0),
            });
        }
        Ok(out)
    }

    pub async fn fees(&self, base: &str, quote: &str) -> Result<Fees> {
        let path = format!("/api/v5/account/trade-fee?instType=SPOT&instId={base}-{quote}");
        let data = self.get(&path).await?;
        let d = &data[0];
        let usdc = quote == "USDC" && num(&d["takerUSDC"]).is_some();
        let (maker, taker) = if usdc {
            (&d["makerUSDC"], &d["takerUSDC"])
        } else {
            (&d["maker"], &d["taker"])
        };
        Ok(Fees {
            maker: -num(maker).context("OKX: brak prowizji maker")?,
            taker: -num(taker).context("OKX: brak prowizji taker")?,
        })
    }

    pub async fn rules(&self, base: &str, quote: &str) -> Result<Rules> {
        let path = format!("/api/v5/public/instruments?instType=SPOT&instId={base}-{quote}");
        let data = retry_rate_limited(|| async {
            let body = self
                .http
                .get(format!("{BASE}{path}"))
                .send()
                .await?
                .text()
                .await?;
            data(&path, &body)
        })
        .await?;
        let d = &data[0];
        Ok(Rules {
            tick: num(&d["tickSz"]).context("OKX: brak tickSz")?,
            step: num(&d["lotSz"]).context("OKX: brak lotSz")?,
            min_qty: num(&d["minSz"]).context("OKX: brak minSz")?,
            min_notional: None,
        })
    }

    pub async fn networks(&self, asset: &str) -> Result<Vec<Network>> {
        let data = self
            .get(&format!("/api/v5/asset/currencies?ccy={asset}"))
            .await?;
        Ok(data
            .as_array()
            .into_iter()
            .flatten()
            .map(|d| Network {
                chain: d["chain"].as_str().unwrap_or_default().to_owned(),
                can_deposit: d["canDep"] == true,
                can_withdraw: d["canWd"] == true,
                withdraw_fee: num(&d["minFee"]).or_else(|| num(&d["fee"])),
                withdraw_min: num(&d["minWd"]),
                withdraw_decimals: num(&d["wdTickSz"]).map(|n| n as u32),
                confirmations: num(&d["minDepArrivalConfirm"]).map(|c| c as u32),
            })
            .collect())
    }

    pub async fn key_rights(&self) -> Result<KeyRights> {
        let data = self.get("/api/v5/account/config").await?;
        let perm = data[0]["perm"].as_str().unwrap_or_default();
        Ok(KeyRights {
            trade: perm.contains("trade"),
            withdraw: perm.contains("withdraw"),
        })
    }

    pub async fn place_ioc(
        &self,
        base: &str,
        quote: &str,
        side: Side,
        qty: &str,
        price: &str,
        client_id: &str,
    ) -> Result<()> {
        let body = json!({
            "instId": format!("{base}-{quote}"),
            "tdMode": "cash",
            "clOrdId": client_id,
            "side": side_id(side),
            "ordType": "ioc",
            "px": price,
            "sz": qty,
        });
        let data = self.post("/api/v5/trade/order", &body.to_string()).await?;
        if data[0]["sCode"] != "0" {
            bail!(
                "OKX odrzucił zlecenie: {} {}",
                data[0]["sCode"],
                data[0]["sMsg"]
            );
        }
        Ok(())
    }

    pub async fn place_post_only(
        &self,
        base: &str,
        quote: &str,
        side: Side,
        qty: &str,
        price: &str,
        client_id: &str,
    ) -> Result<()> {
        let body = json!({
            "instId": format!("{base}-{quote}"),
            "tdMode": "cash",
            "clOrdId": client_id,
            "side": side_id(side),
            "ordType": "post_only",
            "px": price,
            "sz": qty,
        });
        let data = self.post("/api/v5/trade/order", &body.to_string()).await?;
        if data[0]["sCode"] != "0" {
            bail!(
                "OKX odrzucił zlecenie: {} {}",
                data[0]["sCode"],
                data[0]["sMsg"]
            );
        }
        Ok(())
    }

    pub async fn cancel(&self, base: &str, quote: &str, client_id: &str) -> Result<()> {
        let body = json!({"instId": format!("{base}-{quote}"), "clOrdId": client_id});
        match self
            .post("/api/v5/trade/cancel-order", &body.to_string())
            .await
        {
            Err(e) if ALREADY_FINAL.iter().any(|c| format!("{e}").contains(c)) => Ok(()),
            other => other.map(|_| ()),
        }
    }

    pub async fn deposit_addresses(&self, ccy: &str) -> Result<Vec<DepositAddress>> {
        let data = self
            .get(&format!("/api/v5/asset/deposit-address?ccy={ccy}"))
            .await?;
        Ok(data
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| {
                Some(DepositAddress {
                    chain: d["chain"].as_str()?.to_owned(),
                    address: d["addr"].as_str()?.to_owned(),
                    tag: ["tag", "memo", "pmtId"]
                        .iter()
                        .filter_map(|k| d[*k].as_str())
                        .find(|t| !t.is_empty())
                        .map(str::to_owned),
                    to_trading: d["to"] == "18",
                })
            })
            .collect())
    }

    pub async fn withdraw(
        &self,
        ccy: &str,
        amount: &str,
        chain: &str,
        to: &DepositAddress,
        client_id: &str,
        rcvr_info: Option<&Value>,
    ) -> Result<String> {
        let address = match &to.tag {
            Some(tag) => format!("{}:{tag}", to.address),
            None => to.address.clone(),
        };
        let mut body = json!({
            "ccy": ccy,
            "amt": amount,
            "dest": "4",
            "toAddr": address,
            "chain": chain,
            "clientId": client_id,
        });
        if let Some(info) = rcvr_info {
            body["rcvrInfo"] = info.clone();
        }
        let data = self
            .post("/api/v5/asset/withdrawal", &body.to_string())
            .await?;
        data[0]["wdId"]
            .as_str()
            .map(str::to_owned)
            .context("OKX: wypłata bez identyfikatora")
    }

    pub async fn transfer(&self, ccy: &str, amount: &str, to: crate::Wallet) -> Result<()> {
        let (from, to) = match to {
            crate::Wallet::Trading => ("6", "18"),
            crate::Wallet::Funding => ("18", "6"),
        };
        let body = json!({"ccy": ccy, "amt": amount, "from": from, "to": to});
        self.post("/api/v5/asset/transfer", &body.to_string())
            .await
            .map(|_| ())
    }

    pub async fn deposits(&self, ccy: &str) -> Result<Vec<Deposit>> {
        let data = self
            .get(&format!("/api/v5/asset/deposit-history?ccy={ccy}"))
            .await?;
        Ok(data
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| {
                Some(Deposit {
                    amount: num(&d["amt"])?,
                    ts_ms: num(&d["ts"])?,
                    credited: matches!(d["state"].as_str(), Some("1" | "2")),
                })
            })
            .collect())
    }

    pub async fn open_orders(&self, base: &str, quote: &str) -> Result<Vec<String>> {
        let path = format!("/api/v5/trade/orders-pending?instType=SPOT&instId={base}-{quote}");
        let data = self.get(&path).await?;
        Ok(data
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| d["clOrdId"].as_str().map(str::to_owned))
            .collect())
    }

    pub async fn cancel_all_after(&self, secs: u32) -> Result<()> {
        let body = json!({"timeOut": secs.to_string()});
        self.post("/api/v5/trade/cancel-all-after", &body.to_string())
            .await
            .map(|_| ())
    }

    pub async fn order_state(
        &self,
        base: &str,
        quote: &str,
        client_id: &str,
    ) -> Result<OrderUpdate> {
        let path = format!("/api/v5/trade/order?instId={base}-{quote}&clOrdId={client_id}");
        let data = self.get(&path).await?;
        parse_update(&data[0]).with_context(|| format!("OKX: brak zlecenia {client_id}"))
    }

    pub async fn order_updates(
        &self,
        base: &str,
        quote: &str,
        mut on_event: impl FnMut(OrderEvent),
    ) -> Result<()> {
        let (mut ws, _) = connect_async(WS_PRIVATE)
            .await
            .context("połączenie z prywatnym WebSocketem OKX")?;
        let timestamp = (sign::now_ms() / 1000).to_string();
        let login = json!({"op": "login", "args": [{
            "apiKey": self.creds.api_key,
            "passphrase": self.creds.passphrase.as_deref().context("OKX wymaga passphrase")?,
            "timestamp": timestamp,
            "sign": sign::okx(&self.creds.secret, &timestamp, "GET", "/users/self/verify", ""),
        }]});
        ws.send(Message::text(login.to_string())).await?;
        let subscribe = json!({"op": "subscribe", "args": [{
            "channel": "orders", "instType": "SPOT", "instId": format!("{base}-{quote}"),
        }]});
        let mut pinged = false;
        loop {
            let message = match timeout(WS_PING, ws.next()).await {
                Ok(Some(message)) => message?,
                Ok(None) => bail!("OKX zamknął połączenie"),
                Err(_) if pinged => bail!("OKX nie odpowiada na ping"),
                Err(_) => {
                    ws.send(Message::text("ping")).await?;
                    pinged = true;
                    continue;
                }
            };
            pinged = false;
            let Message::Text(text) = message else {
                continue;
            };
            if text.as_str() == "pong" {
                continue;
            }
            let v: Value = serde_json::from_str(text.as_str())?;
            match v["event"].as_str() {
                Some("login") => ws.send(Message::text(subscribe.to_string())).await?,
                Some("subscribe") => on_event(OrderEvent::Subscribed),
                Some("error") => bail!("OKX: {} {}", v["code"], v["msg"]),
                _ => {
                    for d in v["data"].as_array().into_iter().flatten() {
                        if let Some(update) = parse_update(d) {
                            on_event(OrderEvent::Update(update));
                        }
                    }
                }
            }
        }
    }

    pub async fn order(
        &self,
        base: &str,
        quote: &str,
        client_id: &str,
    ) -> Result<Option<OrderFill>> {
        let path = format!("/api/v5/trade/order?instId={base}-{quote}&clOrdId={client_id}");
        let data = self.get(&path).await?;
        let d = &data[0];
        match d["state"].as_str() {
            Some("filled" | "canceled" | "mmp_canceled") => Ok(Some(OrderFill {
                qty: num(&d["accFillSz"]).unwrap_or(0.0),
                avg_price: num(&d["avgPx"]).unwrap_or(0.0),
            })),
            Some(_) => Ok(None),
            None => bail!("OKX: brak zlecenia {client_id}"),
        }
    }

    async fn get(&self, path: &str) -> Result<Value> {
        retry_rate_limited(|| async {
            let body = self.signed(reqwest::Method::GET, path, "").await?;
            data(path, &body)
        })
        .await
    }

    async fn post(&self, path: &str, json: &str) -> Result<Value> {
        let body = self.signed(reqwest::Method::POST, path, json).await?;
        data(path, &body)
    }

    async fn signed(&self, method: reqwest::Method, path: &str, json: &str) -> Result<String> {
        let timestamp = sign::iso_ms(sign::now_ms());
        let signature = sign::okx(&self.creds.secret, &timestamp, method.as_str(), path, json);
        let passphrase = self
            .creds
            .passphrase
            .as_deref()
            .context("OKX wymaga passphrase")?;
        let mut request = self
            .http
            .request(method, format!("{BASE}{path}"))
            .header("OK-ACCESS-KEY", &self.creds.api_key)
            .header("OK-ACCESS-SIGN", signature)
            .header("OK-ACCESS-TIMESTAMP", timestamp)
            .header("OK-ACCESS-PASSPHRASE", passphrase);
        if !json.is_empty() {
            request = request
                .header("Content-Type", "application/json")
                .body(json.to_owned());
        }
        Ok(request.send().await?.text().await?)
    }
}

fn side_id(side: Side) -> &'static str {
    match side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}

fn parse_update(d: &Value) -> Option<OrderUpdate> {
    Some(OrderUpdate {
        client_id: d["clOrdId"].as_str()?.to_owned(),
        side: match d["side"].as_str()? {
            "buy" => Side::Buy,
            "sell" => Side::Sell,
            _ => return None,
        },
        filled: num(&d["accFillSz"]).unwrap_or(0.0),
        avg_price: num(&d["avgPx"]).unwrap_or(0.0),
        done: matches!(d["state"].as_str()?, "filled" | "canceled" | "mmp_canceled"),
    })
}

fn data(path: &str, body: &str) -> Result<Value> {
    let v: Value = serde_json::from_str(body)
        .with_context(|| format!("OKX {path}: odpowiedź nie jest JSON-em"))?;
    if v["code"] != "0" {
        let item = &v["data"][0];
        return Err(Rejected(format!(
            "OKX {path}: kod {} {} {} {}",
            v["code"],
            v["msg"],
            item["sCode"].as_str().unwrap_or_default(),
            item["sMsg"].as_str().unwrap_or_default()
        ))
        .into());
    }
    Ok(v["data"].clone())
}
