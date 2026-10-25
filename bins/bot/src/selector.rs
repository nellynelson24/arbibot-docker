use std::collections::HashMap;
use std::fs;
use std::path::Path;

use chrono::DateTime;
use serde_json::Value;
use venues::{Book, Quote};

const DAY_MS: f64 = 86_400_000.0;
const MIN_DURATION_MS: f64 = 100.0;
pub const MIN_VOLUME_USD: f64 = 250_000.0;
pub const MAX_SPREAD_BPS: f64 = 30.0;
pub const MAX_GAP_BPS: f64 = 100.0;
const VENUE_NAMES: [&str; 2] = ["OKX", "Bybit"];

pub struct Candidate {
    pub pair: String,
    pub profit: f64,
    pub gaps: usize,
    pub buy_okx: usize,
    pub buy_bybit: usize,
}

pub fn rank(
    data_dir: &Path,
    now_ms: f64,
    days: u32,
    fee_bps: f64,
    min_net_bps: f64,
    size: f64,
) -> Vec<Candidate> {
    let since = now_ms - f64::from(days) * DAY_MS;
    let mut by_pair: HashMap<String, Candidate> = HashMap::new();
    for day in (0..=days).map(|d| since + f64::from(d) * DAY_MS) {
        let path = data_dir
            .join("opportunities")
            .join(format!("{}.jsonl", utc_date(day)));
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        for v in text
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        {
            let (Some(buy), Some(sell), Some(pair)) =
                (v["buy"].as_str(), v["sell"].as_str(), v["pair"].as_str())
            else {
                continue;
            };
            if !matches!((buy, sell), ("okx", "bybit") | ("bybit", "okx")) {
                continue;
            }
            let (Some(start), Some(duration), Some(gross)) = (
                v["start_ms"].as_f64(),
                v["duration_ms"].as_f64(),
                v["first_gross_bps"].as_f64(),
            ) else {
                continue;
            };
            let net = gross - fee_bps;
            if start < since || duration < MIN_DURATION_MS || net <= 0.0 || net < min_net_bps {
                continue;
            }
            let c = by_pair.entry(pair.to_owned()).or_insert_with(|| Candidate {
                pair: pair.to_owned(),
                profit: 0.0,
                gaps: 0,
                buy_okx: 0,
                buy_bybit: 0,
            });
            let offer = v["first_size"].as_f64().unwrap_or(size);
            c.profit += net / 1e4 * size.min(offer);
            c.gaps += 1;
            if buy == "okx" {
                c.buy_okx += 1;
            } else {
                c.buy_bybit += 1;
            }
        }
    }
    let mut list: Vec<Candidate> = by_pair.into_values().collect();
    list.sort_by(|a, b| b.profit.total_cmp(&a.profit));
    list
}

pub fn liquidity_issue(pair: &str, books: &[Book; 2]) -> Option<String> {
    let pair = pair.to_uppercase();
    let Some((base, quote)) = pair.split_once('/') else {
        return Some("zły format pary".to_owned());
    };
    let key = (base.to_owned(), quote.to_owned());
    let mut mids = [0.0; 2];
    for (v, name) in VENUE_NAMES.iter().enumerate() {
        let Some(q) = books[v].get(&key).filter(|q| q.bid > 0.0 && q.ask > q.bid) else {
            return Some(format!("brak notowań na {name}"));
        };
        let mid = mid(q);
        let spread = (q.ask - q.bid) / mid * 1e4;
        if spread > MAX_SPREAD_BPS {
            return Some(format!("spread {name} {spread:.0} bp"));
        }
        let Some(usd) = q.volume.zip(usd_rate(quote, books)).map(|(v, r)| v * r) else {
            return Some(format!("brak obrotu {name}"));
        };
        if usd < MIN_VOLUME_USD {
            return Some(format!("obrót {name} {usd:.0} USD"));
        }
        mids[v] = mid;
    }
    let gap = (mids[0] / mids[1] - 1.0).abs() * 1e4;
    (gap > MAX_GAP_BPS).then(|| format!("różnica cen {gap:.0} bp"))
}

fn mid(q: &Quote) -> f64 {
    (q.bid + q.ask) / 2.0
}

fn usd_rate(quote: &str, books: &[Book; 2]) -> Option<f64> {
    if matches!(quote, "USDC" | "USDT" | "USD") {
        return Some(1.0);
    }
    books.iter().find_map(|b| {
        b.get(&("USDC".to_owned(), quote.to_owned()))
            .map(|q| 1.0 / mid(q))
            .or_else(|| b.get(&(quote.to_owned(), "USDC".to_owned())).map(mid))
    })
}

pub fn utc_date(ms: f64) -> String {
    DateTime::from_timestamp_millis(ms as i64)
        .map(|t| t.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quote(bid: f64, ask: f64, volume: f64) -> Quote {
        Quote {
            bid,
            bid_qty: None,
            ask,
            ask_qty: None,
            volume: Some(volume),
        }
    }

    fn books(pairs: &[(&str, Quote, Quote)]) -> [Book; 2] {
        let mut books = [Book::new(), Book::new()];
        for (pair, okx, bybit) in pairs {
            let (base, quote) = pair.split_once('/').unwrap();
            let key = (base.to_owned(), quote.to_owned());
            books[0].insert(key.clone(), Quote { ..*okx });
            books[1].insert(key, Quote { ..*bybit });
        }
        books
    }

    #[test]
    fn liquidity_filter_rejects_thin_wide_or_mispriced_pairs() {
        let b = books(&[
            (
                "ARB/USDC",
                quote(0.1650, 0.1651, 1.17e6),
                quote(0.1650, 0.16502, 3.3e5),
            ),
            (
                "TIA/USDC",
                quote(0.610, 0.611, 4.0e5),
                quote(0.610, 0.6105, 12_410.0),
            ),
            (
                "ANIME/USDC",
                quote(0.0100, 0.0110, 342.0),
                quote(0.0100, 0.0101, 695.0),
            ),
            (
                "GAP/USDC",
                quote(1.000, 1.001, 1e6),
                quote(1.020, 1.021, 1e6),
            ),
            (
                "BTC/PLN",
                quote(400_000.0, 400_100.0, 8e5),
                quote(400_000.0, 400_100.0, 2e6),
            ),
            ("USDC/PLN", quote(3.99, 4.01, 1e6), quote(3.99, 4.01, 1e6)),
        ]);
        assert_eq!(liquidity_issue("ARB/USDC", &b), None);
        assert_eq!(
            liquidity_issue("TIA/USDC", &b).as_deref(),
            Some("obrót Bybit 12410 USD")
        );
        assert_eq!(
            liquidity_issue("ANIME/USDC", &b).as_deref(),
            Some("spread OKX 952 bp")
        );
        assert_eq!(
            liquidity_issue("GAP/USDC", &b).as_deref(),
            Some("różnica cen 196 bp")
        );
        assert_eq!(
            liquidity_issue("BTC/PLN", &b).as_deref(),
            Some("obrót OKX 200000 USD")
        );
        assert_eq!(
            liquidity_issue("XYZ/USDC", &b).as_deref(),
            Some("brak notowań na OKX")
        );
    }

    #[test]
    fn counts_gaps_in_the_window_above_the_threshold() {
        let dir = std::env::temp_dir().join(format!("federbot-selector-{}", std::process::id()));
        fs::create_dir_all(dir.join("opportunities")).unwrap();
        let now = 1_791_720_000_000.0;
        let gap = |pair: &str, buy: &str, sell: &str, start: f64, gross: f64| {
            format!(
                r#"{{"type":"episode","pair":"{pair}","buy":"{buy}","sell":"{sell}","start_ms":{start},"duration_ms":500,"first_gross_bps":{gross},"first_size":100}}"#
            )
        };
        let today = [
            gap("STRK/USDC", "bybit", "okx", now - 3_600_000.0, 75.0),
            gap("STRK/USDC", "okx", "bybit", now - 7_200_000.0, 50.0),
            gap("CHIP/USDC", "okx", "kraken", now - 3_600_000.0, 200.0),
        ];
        let old = [gap("KAIA/USDC", "okx", "bybit", now - 4.0 * DAY_MS, 300.0)];
        fs::write(
            dir.join("opportunities")
                .join(format!("{}.jsonl", utc_date(now))),
            today.join("\n"),
        )
        .unwrap();
        fs::write(
            dir.join("opportunities")
                .join(format!("{}.jsonl", utc_date(now - 4.0 * DAY_MS))),
            old.join("\n"),
        )
        .unwrap();

        let ranked = rank(&dir, now, 3, 35.0, 30.0, 10.0);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].pair, "STRK/USDC");
        assert_eq!((ranked[0].gaps, ranked[0].buy_bybit), (1, 1));
        assert!((ranked[0].profit - 0.04).abs() < 1e-12);

        assert_eq!(rank(&dir, now, 3, 35.0, 0.0, 10.0)[0].gaps, 2);
        fs::remove_dir_all(dir).unwrap();
    }
}
