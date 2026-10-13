use std::fs;

use serde_json::Value;

use super::*;

pub(super) struct TransitRecord {
    pub(super) pair: String,
    pub(super) buy: String,
    pub(super) sell: String,
    pub(super) start_ms: f64,
    pub(super) entry_size: Option<f64>,
    pub(super) alive: bool,
    pub(super) sells: Vec<(Option<f64>, Option<f64>)>,
}

pub(super) fn read_transit(data_dir: &std::path::Path, date: &str) -> (Vec<TransitRecord>, f64) {
    let path = data_dir.join("transit").join(format!("{date}.jsonl"));
    let Ok(text) = fs::read_to_string(path) else {
        return (Vec::new(), 0.0);
    };
    let lines: Vec<Value> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect();
    let secs = status_secs(&lines);
    let mut records: Vec<TransitRecord> = lines
        .iter()
        .filter(|v| v["type"] == "transit")
        .filter_map(|v| {
            Some(TransitRecord {
                pair: v["pair"].as_str()?.to_owned(),
                buy: venue_name(v["buy"].as_str()?),
                sell: venue_name(v["sell"].as_str()?),
                start_ms: v["start_ms"].as_f64()?,
                entry_size: v["entry_size"].as_f64(),
                alive: v["alive"] == true,
                sells: TRANSFER_SECS
                    .iter()
                    .map(|secs| {
                        let at = &v["sells"][secs.to_string()];
                        (at["net_bps"].as_f64(), at["size"].as_f64())
                    })
                    .collect(),
            })
        })
        .collect();
    records.sort_by(|a, b| a.start_ms.total_cmp(&b.start_ms));
    (records, secs)
}

pub(super) fn status_secs(lines: &[Value]) -> f64 {
    lines
        .iter()
        .filter(|v| v["type"] == "status")
        .filter_map(|v| v["interval_secs"].as_f64())
        .sum()
}

pub(super) fn transfer_index(secs: f64) -> usize {
    (0..TRANSFER_SECS.len())
        .min_by(|&a, &b| {
            (TRANSFER_SECS[a] as f64 - secs)
                .abs()
                .total_cmp(&(TRANSFER_SECS[b] as f64 - secs).abs())
        })
        .unwrap_or(0)
}

pub(super) fn nearest_transfer(secs: f64) -> u64 {
    TRANSFER_SECS[transfer_index(secs)]
}

pub(super) fn transit_episodes(records: &[TransitRecord], transfer_secs: f64) -> Vec<Episode> {
    let i = transfer_index(transfer_secs);
    records
        .iter()
        .filter(|r| r.alive)
        .filter_map(|r| {
            let (net_bps, sell_size) = r.sells[i];
            let net_bps = net_bps?;
            let size = match (r.entry_size, sell_size) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            Some(Episode {
                pair: r.pair.clone(),
                buy: r.buy.clone(),
                sell: r.sell.clone(),
                start_ms: r.start_ms,

                duration_ms: f64::INFINITY,
                first_net_bps: net_bps,
                first_size: size,
                best_net_bps: net_bps,
            })
        })
        .collect()
}

pub(super) fn print_transit(records: &[TransitRecord], withdraw_fee: f64) {
    let alive: Vec<&TransitRecord> = records.iter().filter(|r| r.alive).collect();
    if alive.is_empty() {
        return;
    }
    println!(
        "Strategia cykliczna — kupno od razu, sprzedaż po przelewie ({} okazji do kupienia z {}):",
        alive.len(),
        records.len()
    );
    println!(
        "{:>9} {:>11} {:>12} {:>21} {:>21}",
        "Przelew", "Z zyskiem", "Zysk p50", "100 USD: śr./zysk>0", "1000 USD: śr./zysk>0"
    );
    for (i, secs) in TRANSFER_SECS.iter().enumerate() {
        let measured: Vec<(f64, Option<f64>, Option<f64>)> = alive
            .iter()
            .filter_map(|r| r.sells[i].0.map(|net| (net, r.entry_size, r.sells[i].1)))
            .collect();
        if measured.is_empty() {
            continue;
        }
        let nets: Vec<f64> = measured.iter().map(|m| m.0).collect();
        let positive = nets.iter().filter(|&&n| n > 0.0).count();
        let cash = |capital: f64| {
            let per_entry: Vec<f64> = measured
                .iter()
                .map(|&(net, entry, sell)| {
                    let size = capital
                        .min(entry.unwrap_or(capital))
                        .min(sell.unwrap_or(capital));
                    size * net / 1e4 - withdraw_fee
                })
                .collect();
            let mean = per_entry.iter().sum::<f64>() / per_entry.len() as f64;
            let winners = per_entry.iter().filter(|&&c| c > 0.0).count();
            format!(
                "{mean:+.2} / {:.0}%",
                winners as f64 * 100.0 / per_entry.len() as f64
            )
        };
        println!(
            "{:>7} s {:>10.0}% {:>9.1} bp {:>21} {:>21}",
            secs,
            positive as f64 * 100.0 / nets.len() as f64,
            median(&nets),
            cash(100.0),
            cash(1_000.0)
        );
    }
    println!(
        "(zysk po prowizjach taker; kolumny USD: średni zysk na wejście po opłacie wypłaty \
         {withdraw_fee:.2} / odsetek wejść z zyskiem)\n"
    );
}
