use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use serde_json::Value;

use super::stats::pl_hour;
use super::{MIN_CASES, Run, executed, num, text};
use crate::selector::utc_date;

pub(super) fn market_section(
    data_dir: &Path,
    run: Option<&Run>,
    runs: &[Run],
    since: f64,
    until: f64,
    full: &mut String,
    advice: &mut Vec<String>,
) {
    let Some(run) = run else {
        return;
    };
    let pair = run.pair().unwrap_or_else(|| "STRK/USDC".to_owned());
    let min_bps = run.setting("min_bps").unwrap_or(35.0);
    let size = run.setting("size").unwrap_or(20.0);
    let fee_bps = run
        .start
        .as_ref()
        .and_then(|s| s["fees_taker"].as_array())
        .map(|f| f.iter().filter_map(Value::as_f64).fold(0.0, |a, b| a + b) * 1e4)
        .unwrap_or(35.0);

    let mut per_pair: BTreeMap<String, (usize, f64)> = BTreeMap::new();
    let mut hours: BTreeMap<i64, usize> = BTreeMap::new();
    let mut day = since;
    while day <= until + 86_400_000.0 {
        let path = data_dir
            .join("opportunities")
            .join(format!("{}.jsonl", utc_date(day)));
        day += 86_400_000.0;
        let Ok(content) = fs::read_to_string(path) else {
            continue;
        };
        for v in content
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        {
            let start = num(&v, "start_ms");
            let venues = [text(&v, "buy"), text(&v, "sell")];
            if v["type"] != "episode"
                || start < since
                || start > until
                || num(&v, "duration_ms") < 100.0
                || !(venues == ["okx", "bybit"] || venues == ["bybit", "okx"])
            {
                continue;
            }

            let net = v["best_net_bps"]
                .as_f64()
                .unwrap_or_else(|| num(&v, "first_gross_bps") - fee_bps);
            if net < min_bps {
                continue;
            }
            let offer = v["best_size"]
                .as_f64()
                .or_else(|| v["first_size"].as_f64())
                .unwrap_or(size);
            let e = per_pair.entry(text(&v, "pair").to_owned()).or_default();
            e.0 += 1;
            e.1 += net / 1e4 * size.min(offer);
            if text(&v, "pair") == pair {
                *hours.entry(pl_hour(start)).or_default() += 1;
            }
        }
    }
    let ours = per_pair.get(&pair).copied().unwrap_or_default();
    let executed = run.trades().iter().filter(|t| executed(t)).count();
    let _ = writeln!(
        full,
        "## Rynek według skanera (OKX ↔ Bybit, okazje, które w szczycie miały ≥ {min_bps} bp na czysto, ≥ 100 ms)\n\n- {pair}: {} okazji, szacunek {:+.3} USDC przy {size} USD; {} ({}) wykonał {executed} transakcji",
        ours.0,
        ours.1,
        run.label,
        run.settings()
    );
    if !hours.is_empty() {
        let _ = writeln!(
            full,
            "- Godziny okazji {pair} (czas polski): {}",
            hours
                .iter()
                .map(|(h, n)| format!("{h:02}:00 {n}"))
                .collect::<Vec<_>>()
                .join(" | ")
        );
    }
    let mut ranked: Vec<(&String, &(usize, f64))> = per_pair.iter().collect();
    ranked.sort_by(|a, b| b.1.1.total_cmp(&a.1.1));
    let _ = writeln!(
        full,
        "\n| Para | okazje | szacunek USDC (szczyty luk, bez przelewów) |\n|---|---|---|"
    );
    for (p, (n, profit)) in ranked.iter().take(8) {
        let _ = writeln!(full, "| {p} | {n} | {profit:+.3} |");
    }
    let _ = writeln!(full);
    if let Some((best, (n, profit))) = ranked.first()
        && **best != pair
        && *n >= MIN_CASES
        && *profit > 2.0 * ours.1
    {
        let tested = runs
            .iter()
            .find(|r| r.pair().as_deref() == Some(best.as_str()));
        let verdict = match tested {
            Some(r) if r.total() <= 0.0 => format!(
                "Na sucho ({}) {best} dał {:+.3} z kosztami przelewów: na razie się nie opłaca.",
                r.label,
                r.total()
            ),
            Some(r) => format!(
                "Na sucho ({}) {best} dał {:+.3} z kosztami przelewów. Jeśli powtórzy się kilka dni, rozważ go jako drugą parę.",
                r.label,
                r.total()
            ),
            None => "Sprawdź go najpierw na sucho.".to_owned(),
        };
        advice.push(format!(
            "Skaner: {best} miał {n} okazji (szacunek {profit:+.3} ze szczytów luk, bez przelewów) wobec {pair} ({:+.3}). {verdict}",
            ours.1
        ));
    }
}
