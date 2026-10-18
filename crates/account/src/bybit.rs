use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde_json::{Value, json};

use crate::{
    Balance, Credentials, Deposit, DepositAddress, Fees, KeyRights, Network, OrderFill, Rules,
    Side, num, retry_rate_limited, sign,
};

const BASE: &str = "https://api.bybit.eu";
const RECV_WINDOW: u64 = 5_000;

pub struct Bybit {
    http: Client,
    creds: Credentials,
}

impl Bybit {
    pub const TIME_URL: &str = "https://api.bybit.eu/v5/market/time";

    pub fn new(http: Client, creds: Credentials) -> Self {
        Bybit { http, creds }
    }

    pub async fn balances(&self) -> Result<Vec<Balance>> {
        let unified = self
            .get("/v5/account/wallet-balance", "accountType=UNIFIED")
            .await?;
        let funding = self
            .get(
                "/v5/asset/transfer/query-account-coins-balance",
                "accountType=FUND",
            )
            .await?;
        let mut out = Vec::new();
        for c in unified["list"][0]["coin"].as_array().into_iter().flatten() {
            let total = num(&c["walletBalance"]).unwrap_or(0.0);
            out.push(Balance {
                account: "trading",
                asset: c["coin"].as_str().unwrap_or_default().to_owned(),
                total,
                available: total - num(&c["locked"]).unwrap_or(0.0),
            });
        }
        for c in funding["balance"].as_array().into_iter().flatten() {
            out.push(Balance {
                account: "funding",
                asset: c["coin"].as_str().unwrap_or_default().to_owned(),
                total: num(&c["walletBalance"]).unwrap_or(0.0),
                available: num(&c["transferBalance"]).unwrap_or(0.0),
            });
        }
        Ok(out)
    }

    pub async fn fees(&self, base: &str, quote: &str) -> Result<Fees> {
        let result = self
            .get(
                "/v5/account/fee-rate",
                &format!("category=spot&symbol={base}{quote}"),
            )
            .await?;
        let d = &result["list"][0];
        Ok(Fees {
            maker: num(&d["makerFeeRate"]).context("Bybit: brak prowizji maker")?,
            taker: num(&d["takerFeeRate"]).context("Bybit: brak prowizji taker")?,
        })
    }

    pub async fn rules(&self, base: &str, quote: &str) -> Result<Rules> {
        let path = "/v5/market/instruments-info";
        let query = format!("category=spot&symbol={base}{quote}");
        let result = retry_rate_limited(|| async {
            let body = self
                .http
                .get(format!("{BASE}{path}?{query}"))
                .send()
                .await?
                .text()
                .await?;
            result(path, &body)
        })
        .await?;
        let d = &result["list"][0];
        Ok(Rules {
            tick: num(&d["priceFilter"]["tickSize"]).context("Bybit: brak tickSize")?,
            step: num(&d["lotSizeFilter"]["basePrecision"]).context("Bybit: brak basePrecision")?,
            min_qty: num(&d["lotSizeFilter"]["minOrderQty"]).context("Bybit: brak minOrderQty")?,
            min_notional: num(&d["lotSizeFilter"]["minOrderAmt"]),
        })
    }

    pub async fn networks(&self, asset: &str) -> Result<Vec<Network>> {
        let result = self
            .get("/v5/asset/coin/query-info", &format!("coin={asset}"))
            .await?;
        Ok(result["rows"][0]["chains"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| Network {
                chain: c["chain"].as_str().unwrap_or_default().to_owned(),
                can_deposit: c["chainDeposit"] == "1",
                can_withdraw: c["chainWithdraw"] == "1",
                withdraw_fee: num(&c["withdrawFee"]),
                withdraw_min: num(&c["withdrawMin"]),
                withdraw_decimals: num(&c["minAccuracy"]).map(|n| n as u32),
                confirmations: num(&c["confirmation"]).map(|n| n as u32),
            })
            .collect())
    }

    pub async fn deposit_addresses(&self, coin: &str) -> Result<Vec<DepositAddress>> {
        let result = self
            .get("/v5/asset/deposit/query-address", &format!("coin={coin}"))
            .await?;
        Ok(result["chains"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| {
                Some(DepositAddress {
                    chain: c["chain"].as_str()?.to_owned(),
                    address: c["addressDeposit"].as_str()?.to_owned(),
                    tag: c["tagDeposit"]
                        .as_str()
                        .filter(|t| !t.is_empty())
                        .map(str::to_owned),
                    to_trading: false,
                })
            })
            .collect())
    }

    pub async fn withdraw(
        &self,
        coin: &str,
        chain: &str,
        to: &DepositAddress,
        amount: &str,
        request_id: &str,
    ) -> Result<String> {
        let mut body = json!({
            "coin": coin,
            "chain": chain,
            "address": to.address,
            "amount": amount,
            "timestamp": sign::now_ms(),
            "forceChain": 1,
            "accountType": "FUND,UTA",
            "feeType": 1,
            "requestId": request_id,
        });
        if let Some(tag) = &to.tag {
            body["tag"] = json!(tag);
        }
        let result = self
            .post("/v5/asset/withdraw/create", &body.to_string())
            .await?;
        result["id"]
            .as_str()
            .map(str::to_owned)
            .context("Bybit: wypłata bez identyfikatora")
    }

    pub async fn transfer_to_trading(
        &self,
        coin: &str,
        amount: &str,
        transfer_id: &str,
    ) -> Result<()> {
        let body = json!({
            "transferId": transfer_id,
            "coin": coin,
            "amount": amount,
            "fromAccountType": "FUND",
            "toAccountType": "UNIFIED",
        });
        self.post("/v5/asset/transfer/inter-transfer", &body.to_string())
            .await
            .map(|_| ())
    }

    pub async fn deposits(&self, coin: &str) -> Result<Vec<Deposit>> {
        let result = self
            .get("/v5/asset/deposit/query-record", &format!("coin={coin}"))
            .await?;
        Ok(result["rows"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| {
                Some(Deposit {
                    amount: num(&d["amount"])?,
                    ts_ms: num(&d["successAt"]).unwrap_or(0.0),
                    credited: d["status"] == 3 || d["status"] == "3",
                })
            })
            .collect())
    }

    pub async fn key_rights(&self) -> Result<KeyRights> {
        let r = self.get("/v5/user/query-api", "").await?;
        let perms = &r["permissions"];
        let has = |group: &str, name: &str| {
            perms[group]
                .as_array()
                .is_some_and(|list| list.iter().any(|p| p == name))
        };
        let withdraw = perms.as_object().is_some_and(|groups| {
            groups
                .values()
                .flat_map(|l| l.as_array().into_iter().flatten())
                .any(|p| {
                    p.as_str()
                        .is_some_and(|p| p.to_lowercase().contains("withdraw"))
                })
        });
        Ok(KeyRights {
            trade: r["readOnly"] == 0 && has("Spot", "SpotTrade"),
            withdraw,
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
            "category": "spot",
            "symbol": format!("{base}{quote}"),
            "side": match side { Side::Buy => "Buy", Side::Sell => "Sell" },
            "orderType": "Limit",
            "qty": qty,
            "price": price,
            "timeInForce": "IOC",
            "orderLinkId": client_id,
        });
        self.post("/v5/order/create", &body.to_string()).await?;
        Ok(())
    }

    pub async fn order(
        &self,
        base: &str,
        quote: &str,
        client_id: &str,
    ) -> Result<Option<OrderFill>> {
        let query = format!("category=spot&symbol={base}{quote}&orderLinkId={client_id}");
        let mut r = self.get("/v5/order/realtime", &query).await?;
        if r["list"].as_array().is_none_or(|l| l.is_empty()) {
            r = self.get("/v5/order/history", &query).await?;
        }
        let d = &r["list"][0];
        match d["orderStatus"].as_str() {
            Some(
                "Filled" | "Cancelled" | "PartiallyFilledCanceled" | "Rejected" | "Deactivated",
            ) => Ok(Some(OrderFill {
                qty: num(&d["cumExecQty"]).unwrap_or(0.0),
                avg_price: num(&d["avgPrice"]).unwrap_or(0.0),
            })),
            _ => Ok(None),
        }
    }

    async fn get(&self, path: &str, query: &str) -> Result<Value> {
        let url = if query.is_empty() {
            format!("{BASE}{path}")
        } else {
            format!("{BASE}{path}?{query}")
        };
        retry_rate_limited(|| async {
            let body = self.signed(self.http.get(&url), query).await?;
            result(path, &body)
        })
        .await
    }

    async fn post(&self, path: &str, json: &str) -> Result<Value> {
        let request = self
            .http
            .post(format!("{BASE}{path}"))
            .header("Content-Type", "application/json")
            .body(json.to_owned());
        let body = self.signed(request, json).await?;
        result(path, &body)
    }

    async fn signed(&self, request: reqwest::RequestBuilder, payload: &str) -> Result<String> {
        let timestamp = sign::now_ms();
        let signature = sign::bybit(
            &self.creds.secret,
            timestamp,
            &self.creds.api_key,
            RECV_WINDOW,
            payload,
        );
        Ok(request
            .header("X-BAPI-API-KEY", &self.creds.api_key)
            .header("X-BAPI-TIMESTAMP", timestamp.to_string())
            .header("X-BAPI-RECV-WINDOW", RECV_WINDOW.to_string())
            .header("X-BAPI-SIGN", signature)
            .send()
            .await?
            .text()
            .await?)
    }
}

fn result(path: &str, body: &str) -> Result<Value> {
    let v: Value = serde_json::from_str(body)
        .with_context(|| format!("Bybit {path}: odpowiedź nie jest JSON-em"))?;
    if v["retCode"] != 0 {
        bail!("Bybit {path}: kod {} {}", v["retCode"], v["retMsg"]);
    }
    Ok(v["result"].clone())
}
