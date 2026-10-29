mod bybit;
mod credentials;
mod okx;
mod sign;

pub use bybit::Bybit;
pub use credentials::{Credentials, Scope, secret};
pub use okx::{Okx, OrderEvent, OrderUpdate};

#[derive(Debug)]
pub struct Rejected(pub String);

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Rejected {}

pub fn rejected(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<Rejected>().is_some())
}

#[derive(Debug, Clone, Copy)]
pub struct Deposit {
    pub amount: f64,
    pub ts_ms: f64,
    pub credited: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wallet {
    Funding,
    Trading,
}

#[derive(Debug, Clone)]
pub struct DepositAddress {
    pub chain: String,
    pub address: String,
    pub tag: Option<String>,
    pub to_trading: bool,
}

#[derive(Debug, Clone)]
pub struct Balance {
    pub account: &'static str,
    pub asset: String,
    pub total: f64,
    pub available: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct Fees {
    pub maker: f64,
    pub taker: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct OrderFill {
    pub qty: f64,
    pub avg_price: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct KeyRights {
    pub trade: bool,
    pub withdraw: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct Rules {
    pub tick: f64,
    pub step: f64,
    pub min_qty: f64,
    pub min_notional: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct Network {
    pub chain: String,
    pub can_deposit: bool,
    pub can_withdraw: bool,
    pub withdraw_fee: Option<f64>,
    pub withdraw_min: Option<f64>,
    pub withdraw_decimals: Option<u32>,
    pub confirmations: Option<u32>,
}

impl Network {
    pub fn key(&self) -> String {
        let name = self.chain.to_lowercase();
        let name = name
            .split_once('-')
            .map_or(name.as_str(), |(_, chain)| chain);
        let name = name.trim().replace([' ', '_'], "");
        match name.as_str() {
            "sol" | "solana" => "solana",
            "eth" | "erc20" | "ethereum" => "ethereum",
            "bsc" | "bep20" | "bnbsmartchain" | "bnbchain" => "bsc",
            "trx" | "trc20" | "tron" => "tron",
            "arbi" | "arb" | "arbitrum" | "arbitrumone" => "arbitrum",
            "op" | "optimism" => "optimism",
            "matic" | "polygon" | "polygonpos" => "polygon",
            "avaxc" | "avalanchec" | "avalanchec-chain" | "avalanchecchain" => "avalanche-c",
            "base" => "base",
            "kaia" | "klay" | "klaytn" => "kaia",
            other => return other.to_owned(),
        }
        .to_owned()
    }
}

pub(crate) fn num(v: &serde_json::Value) -> Option<f64> {
    let x = match v {
        serde_json::Value::String(s) if !s.is_empty() => s.parse().ok()?,
        serde_json::Value::Number(n) => n.as_f64()?,
        _ => return None,
    };
    x.is_finite().then_some(x)
}

const RETRIES: u32 = 5;
const RATE_LIMITED: [&str; 4] = ["50011", "50061", "10006", "10018"];

pub(crate) async fn retry_rate_limited<F, Fut>(mut call: F) -> anyhow::Result<serde_json::Value>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<serde_json::Value>>,
{
    let mut pause = std::time::Duration::from_millis(300);
    for _ in 1..RETRIES {
        match call().await {
            Err(e) if rate_limited(&e) => {
                let jitter = std::time::Duration::from_millis(sign::now_ms() % 200);
                tokio::time::sleep(pause + jitter).await;
                pause *= 2;
            }
            other => return other,
        }
    }
    call().await
}

fn rate_limited(e: &anyhow::Error) -> bool {
    let text = format!("{e:#}");
    RATE_LIMITED.iter().any(|code| text.contains(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_refusals_are_told_apart_from_unknown_outcomes() {
        let refused =
            anyhow::Error::from(Rejected("Bybit /v5/order/create: kod 170131".to_owned()))
                .context("Bybit EU: zlecenie sprzedaży 97.2 po 0.15464");
        assert!(rejected(&refused));
        assert!(format!("{refused:#}").contains("170131"));
        assert!(!rejected(&anyhow::anyhow!(
            "Bybit EU: zlecenie fb1x2 bez wyniku po 5 s"
        )));
    }

    #[test]
    fn rate_limit_errors_are_recognised() {
        assert!(rate_limited(&anyhow::anyhow!(
            "OKX /x: kod \"50011\" \"Too many requests\""
        )));
        assert!(rate_limited(&anyhow::anyhow!(
            "Bybit /y: kod 10006 \"Too many visits\""
        )));
        assert!(!rate_limited(&anyhow::anyhow!(
            "OKX /x: kod \"51603\" \"Order does not exist\""
        )));
    }
}
