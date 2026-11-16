use std::fmt::Write as _;

use serde_json::Value;

use super::stats::mean;
use super::{MIN_CASES, Run, SAME_SIGNAL_MS, executed, num, text, ts};

pub(super) fn variants_section(
    runs: &[Run],
    full: &mut String,
    short: &mut String,
    advice: &mut Vec<String>,
) {
    let _ = writeln!(
        full,
        "## Wszystkie warianty w tym samym oknie\n\n| Wariant | Ustawienia | sygnały | wykonane | puste | na plus | taker USDC | śr. bp | maker zabezp. | maker USDC | rebalans | razem USDC |\n|---|---|---|---|---|---|---|---|---|---|---|---|"
    );
    let mut totals = Vec::new();
    for run in runs {
        let trades = run.trades();
        let done: Vec<&&Value> = trades.iter().filter(|t| executed(t)).collect();
        let hedges = run.of("maker_hedge");
        let usdc = run.usdc();
        let (taker, maker) = (
            run.sum("trade", "pnl") * usdc,
            run.sum("maker_hedge", "pnl") * usdc,
        );
        let (rebalance, total) = (run.rebalance_cost() * usdc, run.total() * usdc);
        let _ = writeln!(
            full,
            "| {} | {} | {} | {} | {} | {} | {:+.4} | {:+.1} | {} | {:+.4} | {:.4} | **{:+.4}** |",
            run.label,
            run.settings(),
            trades.len(),
            done.len(),
            trades.len() - done.len(),
            trades.iter().filter(|t| num(t, "pnl") > 0.0).count(),
            taker,
            mean(done.iter().map(|t| num(t, "pnl_bps"))),
            hedges.len(),
            maker,
            rebalance,
            total
        );
        totals.push((run.label.clone(), total, trades.len() + hedges.len()));
    }
    let _ = writeln!(full);
    let _ = writeln!(short, "\nWarianty (razem USDC):");
    let mut ranked = totals.clone();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    for (label, total, n) in ranked.iter().take(10) {
        let _ = writeln!(short, "{label}: {total:+.3} ({n} tr.)");
    }
    let main = totals.iter().find(|t| t.0 == "główny");
    if let (Some(best), Some(main)) = (ranked.first(), main)
        && best.0 != main.0
        && best.2 >= MIN_CASES
        && best.1 > main.1
    {
        advice.push(format!(
            "Najlepszy wariant w oknie: {} ({:+.3} USDC z {} transakcji) wobec głównego {:+.3}. Jedno okno to za mało — zmieniaj ustawienia dopiero, gdy przewaga powtarza się kilka dni.",
            best.0, best.1, best.2, main.1
        ));
    }
}

const MAX_STATUS_GAP_MS: f64 = 300_000.0;

fn pair_hours(run: &Run) -> Vec<(String, f64)> {
    let mut hours: Vec<(String, f64)> = Vec::new();
    let mut pair = run.pair();
    let mut last: Option<f64> = None;
    for row in &run.rows {
        match row["type"].as_str() {
            Some("start") => {
                pair = row["pair"].as_str().map(str::to_owned).or(pair);
                last = None;
            }
            Some("switch") => pair = row["to"].as_str().map(str::to_owned).or(pair),
            Some("status") => {
                pair = row["pair"].as_str().map(str::to_owned).or(pair);
                let dt = last
                    .map(|l| ts(row) - l)
                    .filter(|dt| *dt <= MAX_STATUS_GAP_MS);
                if let (Some(dt), Some(p)) = (dt, &pair) {
                    match hours.iter_mut().find(|(q, _)| q == p) {
                        Some(entry) => entry.1 += dt / 3_600_000.0,
                        None => hours.push((p.clone(), dt / 3_600_000.0)),
                    }
                }
                last = Some(ts(row));
            }
            _ => {}
        }
    }
    hours.sort_by(|a, b| b.1.total_cmp(&a.1));
    hours
}

pub(super) fn switch_section(runs: &[Run], full: &mut String, short: &mut String) {
    let switching: Vec<&Run> = runs
        .iter()
        .filter(|r| {
            r.start.as_ref().is_some_and(|s| s["switch"] == true) || !r.of("switch").is_empty()
        })
        .collect();
    if switching.is_empty() {
        return;
    }
    let _ = writeln!(
        full,
        "## Zmiana pary\n\n| Wariant | zmiany | koszt USDC | czas na parach |\n|---|---|---|---|"
    );
    for run in switching {
        let switches = run.of("switch").len();
        let cost = run.sum("switch", "cost") * run.usdc();
        let time = pair_hours(run)
            .iter()
            .map(|(p, h)| format!("{p} {h:.1} h"))
            .collect::<Vec<_>>()
            .join(", ");
        let time = if time.is_empty() {
            "-".to_owned()
        } else {
            time
        };
        let _ = writeln!(full, "| {} | {switches} | {cost:.4} | {time} |", run.label);
        let _ = writeln!(
            short,
            "{}: zmian pary {switches}, koszt {cost:.3}; {time}",
            run.label
        );
    }
    let _ = writeln!(full);
}

pub(super) fn agreement_section(live: &Run, runs: &[Run], full: &mut String) {
    let live_trades = live.trades();
    if live_trades.is_empty() {
        return;
    }
    let _ = writeln!(
        full,
        "## Te same sygnały: na żywo a na sucho\n\n| Dry-run | ten sam sygnał | oba wykonane | tylko na żywo | tylko na sucho | oba puste | na żywo USDC | na sucho USDC |\n|---|---|---|---|---|---|---|---|"
    );
    for run in runs.iter().filter(|r| !r.live) {
        let dry = run.trades();
        let mut counts = [0usize; 5];
        let (mut live_pnl, mut dry_pnl) = (0.0, 0.0);
        for t in &live_trades {
            let twin = dry.iter().find(|d| {
                text(d, "buy") == text(t, "buy") && (ts(d) - ts(t)).abs() <= SAME_SIGNAL_MS
            });
            let Some(twin) = twin else {
                continue;
            };
            counts[0] += 1;
            live_pnl += num(t, "pnl");
            dry_pnl += num(twin, "pnl");
            let i = match (executed(t), executed(twin)) {
                (true, true) => 1,
                (true, false) => 2,
                (false, true) => 3,
                (false, false) => 4,
            };
            counts[i] += 1;
        }
        let _ = writeln!(
            full,
            "| {} | {} | {} | {} | {} | {} | {live_pnl:+.4} | {dry_pnl:+.4} |",
            run.label, counts[0], counts[1], counts[2], counts[3], counts[4]
        );
    }
    let _ = writeln!(full);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn switching_variant_shows_switches_their_cost_and_time_on_each_pair() {
        let t0 = 1_792_800_000_000.0_f64;
        let mut rows =
            vec![json!({"type": "start", "ts_ms": t0, "pair": "ARB/USDC", "switch": true})];
        for m in 1..=60 {
            rows.push(json!({"type": "status", "ts_ms": t0 + f64::from(m) * 60_000.0, "pair": "ARB/USDC"}));
        }
        rows.push(json!({"type": "switch", "ts_ms": t0 + 3_610_000.0, "from": "ARB/USDC", "to": "ASTER/USDC", "cost": 0.35}));
        for m in 61..=240 {
            rows.push(json!({"type": "status", "ts_ms": t0 + f64::from(m) * 60_000.0, "pair": "ASTER/USDC"}));
        }
        rows.push(json!({"type": "maker_hedge", "ts_ms": t0 + 9e6, "pnl": 1.0}));
        let run = Run {
            label: "ab-switch".to_owned(),
            live: false,
            start: Some(
                json!({"type": "start", "pair": "ARB/USDC", "switch": true, "review_hours": 1.0, "switch_maker_hours": 12.0}),
            ),
            prior_start: None,
            rows,
        };
        assert!((run.total() - 0.65).abs() < 1e-12);
        assert!(run.settings().contains("zmiana pary co 1 h (maker z 12 h)"));
        let fixed = Run {
            label: "ab-arb".to_owned(),
            live: false,
            start: Some(json!({"type": "start", "pair": "ARB/USDC"})),
            prior_start: None,
            rows: vec![json!({"type": "status", "ts_ms": t0})],
        };
        let (mut full, mut short) = (String::new(), String::new());
        switch_section(&[run, fixed], &mut full, &mut short);
        assert!(
            full.contains("| ab-switch | 1 | 0.3500 | ASTER/USDC 3.0 h, ARB/USDC 1.0 h |"),
            "{full}"
        );
        assert!(!full.contains("ab-arb"));
        assert_eq!(
            short.trim(),
            "ab-switch: zmian pary 1, koszt 0.350; ASTER/USDC 3.0 h, ARB/USDC 1.0 h"
        );
    }
}
