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
        let (taker, maker) = (run.sum("trade", "pnl"), run.sum("maker_hedge", "pnl"));
        let (rebalance, total) = (run.rebalance_cost(), run.total());
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
    for (label, total, n) in ranked.iter().take(5) {
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
