use std::collections::BTreeMap;
use std::fmt::Write as _;

use super::stats::{mean, pct};
use super::{MIN_CASES, Run, SAME_SIGNAL_MS, executed, num, text, ts};

pub(super) fn maker_section(runs: &[Run], full: &mut String, advice: &mut Vec<String>) {
    let makers: Vec<&Run> = runs
        .iter()
        .filter(|r| !r.of("maker_hedge").is_empty())
        .collect();
    if makers.is_empty() {
        return;
    }
    let _ = writeln!(
        full,
        "## Maker na OKX (na sucho)\n\n| Wariant | wypełnienia | zabezpieczenia | suma USDC | śr. bp | mediana bp | na plus | kupno USDC | sprzedaż USDC | niepełne zabezp. |\n|---|---|---|---|---|---|---|---|---|---|"
    );
    for run in makers {
        let hedges = run.of("maker_hedge");
        let mut bps: Vec<f64> = hedges.iter().map(|h| num(h, "pnl_bps")).collect();
        let side = |s: &str| -> f64 {
            hedges
                .iter()
                .filter(|h| text(h, "side") == s)
                .map(|h| num(h, "pnl"))
                .fold(0.0, |a, b| a + b)
        };
        let positive = hedges.iter().filter(|h| num(h, "pnl") > 0.0).count();
        let mean_bps = mean(bps.iter().copied());
        let _ = writeln!(
            full,
            "| {} | {} | {} | {:+.4} | {mean_bps:+.1} | {:+.1} | {positive}/{} | {:+.4} | {:+.4} | {} |",
            run.label,
            run.of("maker_fill").len(),
            hedges.len(),
            hedges.iter().map(|h| num(h, "pnl")).fold(0.0, |a, b| a + b),
            pct(&mut bps, 50.0),
            hedges.len(),
            side("buy"),
            side("sell"),
            hedges.iter().filter(|h| num(h, "missed") > 0.0).count()
        );
        if hedges.len() >= 30 {
            if mean_bps > 0.0 && positive * 10 >= hedges.len() * 6 {
                advice.push(format!(
                    "Maker ({}) na plus: {} zabezpieczeń, średnio {mean_bps:+.1} bp, {positive} na plus.",
                    run.label,
                    hedges.len()
                ));
            } else if mean_bps < 0.0 {
                advice.push(format!(
                    "Maker ({}) na minus: średnio {mean_bps:+.1} bp z {} zabezpieczeń — niekorzystne wypełnienia zjadają marżę; rozważ wyższą marżę makera.",
                    run.label,
                    hedges.len()
                ));
            }
        }
    }
    let _ = writeln!(full);
}

struct Signal {
    direction: String,
    time: f64,
    expected: f64,
    results: Vec<(f64, bool)>,
}

pub(super) fn calibration_section(runs: &[Run], full: &mut String, advice: &mut Vec<String>) {
    let mut signals: Vec<Signal> = Vec::new();
    for run in runs {
        for t in run.trades() {
            let outcome = (num(t, "pnl_bps"), executed(t));
            match signals
                .iter_mut()
                .find(|s| s.direction == text(t, "buy") && (s.time - ts(t)).abs() <= SAME_SIGNAL_MS)
            {
                Some(signal) => signal.results.push(outcome),
                None => signals.push(Signal {
                    direction: text(t, "buy").to_owned(),
                    time: ts(t),
                    expected: num(t, "expected_bps"),
                    results: vec![outcome],
                }),
            }
        }
    }
    if signals.is_empty() {
        return;
    }

    let mut buckets: BTreeMap<i64, Vec<(f64, f64, bool)>> = BTreeMap::new();
    for signal in &signals {
        let bucket = ((signal.expected / 10.0).floor() as i64 * 10).min(70);
        let result = mean(signal.results.iter().map(|r| r.0));
        let filled =
            signal.results.iter().filter(|r| r.1).count() as f64 / signal.results.len() as f64;
        buckets
            .entry(bucket)
            .or_default()
            .push((result, filled, result > 0.0));
    }
    let _ = writeln!(
        full,
        "## Oczekiwany zysk a wynik\n\nKażdy sygnał liczony raz (wynik uśredniony po wariantach, które go wzięły).\n\n| Oczekiwane bp | różne sygnały | śr. wynik bp (puste = 0) | śr. udział wypełnionych | na plus |\n|---|---|---|---|---|"
    );
    for (bucket, xs) in &buckets {
        let result = mean(xs.iter().map(|x| x.0));
        let _ = writeln!(
            full,
            "| {bucket}–{} | {} | {result:+.1} | {:.0}% | {}/{} |",
            bucket + 10,
            xs.len(),
            mean(xs.iter().map(|x| x.1)) * 100.0,
            xs.iter().filter(|x| x.2).count(),
            xs.len()
        );
        if xs.len() >= MIN_CASES && result < 0.0 {
            advice.push(format!(
                "Sygnały z oczekiwanym zyskiem {bucket}–{} bp średnio tracą ({result:+.1} bp z {} różnych sygnałów).",
                bucket + 10,
                xs.len()
            ));
        }
    }
    let _ = writeln!(full);
}
