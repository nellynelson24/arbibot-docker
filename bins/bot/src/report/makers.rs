use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use serde_json::Value;
use venues::Book;

use super::stats::mean;
use super::{MIN_CASES, Run, num, text, ts};
use crate::selector::{liquidity_issue, utc_date};

const DAY_MS: f64 = 86_400_000.0;
const SHOWN: usize = 8;
const SWITCH_COST: f64 = 0.4;
const PAYBACK_DAYS: f64 = 7.0;

#[derive(Default)]
struct PairFills {
    fills: usize,
    unhedged: usize,
    positive: usize,
    bps: Vec<f64>,
}

impl PairFills {
    fn per_day(&self, size: f64, days: f64) -> f64 {
        self.bps
            .iter()
            .map(|b| b / 1e4 * size)
            .fold(0.0, |a, b| a + b)
            / days
    }
}

pub(super) fn scanner_maker_section(
    (data_dir, books): (&Path, Option<&[Book; 2]>),
    run: Option<&Run>,
    (since, until): (f64, f64),
    full: &mut String,
    advice: &mut Vec<String>,
) {
    let (fills, covered) = read_fills(data_dir, since, until);
    if fills.is_empty() {
        return;
    }
    let pair = run.and_then(Run::pair).unwrap_or_default();
    let size = run
        .and_then(|r| r.start.as_ref())
        .and_then(|s| s["maker_size"].as_f64())
        .unwrap_or(10.0);
    let days = (covered / DAY_MS).max(1.0 / 24.0);

    let mut ranked: Vec<(&String, &PairFills)> = fills
        .iter()
        .filter(|(p, _)| **p == pair || books.is_none_or(|b| liquidity_issue(p, b).is_none()))
        .collect();
    ranked.sort_by(|a, b| b.1.per_day(size, days).total_cmp(&a.1.per_day(size, days)));

    let _ = writeln!(
        full,
        "## Maker według skanera (zlecenie na OKX, zabezpieczenie na Bybit, symulacja)\n\nSzacunek przy zleceniu {size} USDC z {days:.1} dni danych{}.\n\n| Para | wypełnienia | śr. bp | na plus | bez zabezp. | USDC/dzień |\n|---|---|---|---|---|---|",
        if books.is_some() {
            ", tylko pary po filtrach płynności"
        } else {
            ", bez filtrów płynności (brak notowań)"
        }
    );
    let rows = ranked
        .iter()
        .enumerate()
        .filter(|(i, (p, _))| *i < SHOWN || **p == pair);
    for (_, (p, f)) in rows {
        let mark = if **p == pair { " (obecna)" } else { "" };
        let _ = writeln!(
            full,
            "| {p}{mark} | {} | {:+.1} | {}/{} | {} | {:+.3} |",
            f.fills,
            mean(f.bps.iter().copied()),
            f.positive,
            f.bps.len(),
            f.unhedged,
            f.per_day(size, days)
        );
    }
    let _ = writeln!(full);

    let ours = fills.get(&pair);
    if let (Some(run), Some(sim)) = (run, ours) {
        let live: Vec<f64> = run
            .of("maker_hedge")
            .iter()
            .map(|h| num(h, "pnl_bps"))
            .collect();
        if live.len() >= MIN_CASES && sim.bps.len() >= MIN_CASES {
            let _ = writeln!(
                full,
                "{} {pair}: średnio {:+.1} bp z {} zabezpieczeń wobec {:+.1} bp w symulacji skanera.\n",
                run.label,
                mean(live.iter().copied()),
                live.len(),
                mean(sim.bps.iter().copied())
            );
        }
    }
    let current = ours.map_or(0.0, |f| f.per_day(size, days));
    if let Some((best, f)) = ranked.first()
        && **best != pair
        && f.bps.len() >= MIN_CASES
        && (f.per_day(size, days) - current) * PAYBACK_DAYS >= SWITCH_COST
    {
        advice.push(format!(
            "Skaner (maker OKX → Bybit): {best} ~{:+.3} USDC/dzień wobec {current:+.3} dla {pair}; zmiana pary (~{SWITCH_COST} USDC) zwróci się w tydzień. Sprawdź go najpierw na sucho.",
            f.per_day(size, days)
        ));
    }
}

fn read_fills(data_dir: &Path, since: f64, until: f64) -> (BTreeMap<String, PairFills>, f64) {
    let mut by_pair: BTreeMap<String, PairFills> = BTreeMap::new();
    let (mut first, mut last) = (f64::MAX, f64::MIN);
    let mut day = since;
    while day <= until + DAY_MS {
        let path = data_dir
            .join("maker")
            .join(format!("{}.jsonl", utc_date(day)));
        day += DAY_MS;
        let Ok(content) = fs::read_to_string(path) else {
            continue;
        };
        for v in content
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        {
            let t = ts(&v);
            if t < since || t > until {
                continue;
            }
            (first, last) = (first.min(t), last.max(t));
            if v["type"] != "fill" || text(&v, "maker") != "okx" || text(&v, "hedge") != "bybit" {
                continue;
            }
            let f = by_pair.entry(text(&v, "pair").to_owned()).or_default();
            f.fills += 1;
            match v["edge_bps"].as_f64() {
                Some(bps) => {
                    f.positive += usize::from(bps > 0.0);
                    f.bps.push(bps);
                }
                None => f.unhedged += 1,
            }
        }
    }
    (by_pair, (last - first).max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use venues::Quote;

    #[test]
    fn ranks_liquid_pairs_by_simulated_maker_profit() {
        let dir = std::env::temp_dir().join(format!("arbibot-makers-{}", std::process::id()));
        fs::create_dir_all(dir.join("maker")).unwrap();
        let now = 1_792_900_000_000.0;
        let fill = |pair: &str, maker: &str, bps: Option<f64>| {
            json!({"type": "fill", "ts_ms": now - 3_600_000.0, "pair": pair, "maker": maker,
                "hedge": "bybit", "edge_bps": bps})
            .to_string()
        };
        let mut lines = Vec::new();
        for _ in 0..10 {
            lines.push(fill("ARB/USDC", "okx", Some(20.0)));
            lines.push(fill("WLD/USDC", "okx", Some(30.0)));
            lines.push(fill("ANIME/USDC", "okx", Some(200.0)));
            lines.push(fill("WLD/USDC", "kraken", Some(500.0)));
        }
        lines.push(fill("WLD/USDC", "okx", None));
        for t in [now - DAY_MS + 1.0, now] {
            lines.push(json!({"type": "status", "ts_ms": t}).to_string());
        }
        fs::write(
            dir.join("maker").join(format!("{}.jsonl", utc_date(now))),
            lines.join("\n"),
        )
        .unwrap();

        let quote = |bid: f64, ask: f64, volume: f64| Quote {
            bid,
            bid_qty: None,
            ask,
            ask_qty: None,
            volume: Some(volume),
        };
        let mut books = [Book::new(), Book::new()];
        for (pair, volume) in [("ARB", 1e6), ("WLD", 1e6), ("ANIME", 500.0)] {
            for b in &mut books {
                b.insert(
                    (pair.to_owned(), "USDC".to_owned()),
                    quote(1.0, 1.0001, volume),
                );
            }
        }

        let run = Run {
            label: "na żywo".to_owned(),
            live: true,
            start: Some(json!({"type": "start", "pair": "ARB/USDC", "maker_size": 10.0})),
            prior_start: None,
            rows: vec![json!({"type": "start", "ts_ms": now - DAY_MS, "pair": "ARB/USDC"})],
        };
        let (mut full, mut advice) = (String::new(), Vec::new());
        scanner_maker_section(
            (&dir, Some(&books)),
            Some(&run),
            (now - DAY_MS, now),
            &mut full,
            &mut advice,
        );
        fs::remove_dir_all(&dir).unwrap();

        let wld = full.find("| WLD/USDC |").unwrap();
        let arb = full.find("| ARB/USDC (obecna) |").unwrap();
        assert!(wld < arb);
        assert!(!full.contains("ANIME"));
        assert!(full.contains("| WLD/USDC | 11 | +30.0 | 10/10 | 1 | +0.300 |"));
        assert!(full.contains("| ARB/USDC (obecna) | 10 | +20.0 | 10/10 | 0 | +0.200 |"));
        assert_eq!(advice.len(), 1);
        assert!(advice[0].starts_with("Skaner (maker OKX → Bybit): WLD/USDC ~+0.300"));
    }
}
