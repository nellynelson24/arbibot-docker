use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde_json::Value;

use super::stats::{mean, pct, pl_hour, pl_time};
use super::{MIN_CASES, Run, executed, num, text, ts};

pub(super) fn live_section(
    run: &Run,
    full: &mut String,
    short: &mut String,
    advice: &mut Vec<String>,
) {
    let trades = run.trades();
    let done: Vec<&&Value> = trades.iter().filter(|t| executed(t)).collect();
    let pnl: f64 = trades.iter().map(|t| num(t, "pnl")).fold(0.0, |a, b| a + b);
    let mean_bps = mean(done.iter().map(|t| num(t, "pnl_bps")));
    let stalls = run.of("stall");
    let stall_min = stalls
        .iter()
        .map(|s| num(s, "gap_ms"))
        .fold(0.0, |a, b| a + b)
        / 60_000.0;
    let halts = run.of("halt");
    let restarts = run.of("start").len();
    let statuses = run.of("status");

    let _ = writeln!(full, "## Na żywo\n\nUstawienia: {}\n", run.settings());
    let _ = writeln!(
        full,
        "- Arbitraż: **{pnl:+.4} USDC** z {} sygnałów, wykonane {} (średnio {:+.1} bp na wykonaną)",
        trades.len(),
        done.len(),
        mean_bps
    );
    let changed = pair_since(run.prior_start.as_ref(), &run.of("start"));
    let since_pair = changed.unwrap_or(f64::MIN);
    let equity: Vec<&&Value> = statuses.iter().filter(|s| ts(s) >= since_pair).collect();
    if let (Some(first), Some(last)) = (equity.first(), equity.last()) {
        let _ = writeln!(
            full,
            "- Kapitał widziany przez bota{}: {:.2} → {:.2} USDC (bez przelewów w drodze; różnica poza arbitrażem to kurs i przelewy)",
            changed.map_or_else(String::new, |t| format!(
                " od zmiany pary ({})",
                pl_time(t, "%d.%m %H:%M")
            )),
            num(first, "equity"),
            num(last, "equity")
        );
    }
    let _ = writeln!(
        full,
        "- Starty w oknie: {restarts}, uśpienia: {} (razem {stall_min:.0} min), zatrzymania: {}",
        stalls.len(),
        halts.len()
    );
    for h in &halts {
        let _ = writeln!(
            full,
            "  - {} {}",
            pl_time(ts(h), "%d.%m %H:%M"),
            text(h, "reason")
        );
    }
    let hedges = run.of("maker_hedge");
    let maker = run.sum("maker_hedge", "pnl");
    let _ = writeln!(
        short,
        "\nNa żywo: razem {:+.4} USDC\n• taker: {pnl:+.4} z {} wykonanych na {} sygnałów, śr. {mean_bps:+.0} bp\n• maker: {maker:+.4} z {} zabezpieczeń",
        pnl + maker,
        done.len(),
        trades.len(),
        hedges.len()
    );
    if let Some(last) = statuses.last() {
        let _ = writeln!(short, "Kapitał: {:.2} USDC", num(last, "equity"));
    }
    if !halts.is_empty() {
        let _ = writeln!(short, "Zatrzymania: {}", halts.len());
    }

    let _ = writeln!(
        full,
        "\n| Wynik | n | suma USDC | śr. bp |\n|---|---|---|---|"
    );
    for outcome in ["sparowana", "z resztą", "brak wypełnienia"] {
        let xs: Vec<&&Value> = trades
            .iter()
            .filter(|t| text(t, "outcome") == outcome)
            .collect();
        let _ = writeln!(
            full,
            "| {outcome} | {} | {:+.4} | {:+.1} |",
            xs.len(),
            xs.iter().map(|t| num(t, "pnl")).fold(0.0, |a, b| a + b),
            mean(xs.iter().map(|t| num(t, "pnl_bps")))
        );
    }

    let _ = writeln!(
        full,
        "\n| Kierunek | n | wykonane | suma USDC | śr. bp (wykonane) |\n|---|---|---|---|---|"
    );
    for (buy, sell) in [("okx", "bybit"), ("bybit", "okx")] {
        let xs: Vec<&&Value> = trades
            .iter()
            .filter(|t| text(t, "buy") == buy && text(t, "sell") == sell)
            .collect();
        let ok: Vec<&&&Value> = xs.iter().filter(|t| executed(t)).collect();
        let _ = writeln!(
            full,
            "| kup {buy} → sprzedaj {sell} | {} | {} | {:+.4} | {:+.1} |",
            xs.len(),
            ok.len(),
            xs.iter().map(|t| num(t, "pnl")).fold(0.0, |a, b| a + b),
            mean(ok.iter().map(|t| num(t, "pnl_bps")))
        );
    }

    let mut legs: BTreeMap<(String, &str), [usize; 3]> = BTreeMap::new();
    for t in &trades {
        let qty = num(t, "qty");
        for (venue, side, fill) in [
            (text(t, "buy"), "kupno", &t["buy_fill"]),
            (text(t, "sell"), "sprzedaż", &t["sell_fill"]),
        ] {
            let filled = fill[0].as_f64().unwrap_or(0.0);
            let slot = legs.entry((venue.to_owned(), side)).or_default();
            let i = if filled >= qty * 0.999 {
                0
            } else if filled > 0.0 {
                1
            } else {
                2
            };
            slot[i] += 1;
        }
    }
    let _ = writeln!(
        full,
        "\n| Noga | pełne | częściowe | puste |\n|---|---|---|---|"
    );
    for ((venue, side), [whole, part, none]) in &legs {
        let _ = writeln!(full, "| {venue} {side} | {whole} | {part} | {none} |");
    }

    let since_start = run.start.as_ref().map_or(0.0, ts);
    let mut acks: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for row in run.rows.iter().filter(|r| ts(r) >= since_start) {
        for o in row["orders"].as_array().into_iter().flatten() {
            if let Some(ms) = o["ack_ms"].as_f64() {
                acks.entry(text(o, "venue").to_owned())
                    .or_default()
                    .push(ms);
            }
        }
    }
    if acks.is_empty() {
        let _ = writeln!(
            full,
            "\nOd ostatniego startu ({}) nie było jeszcze zleceń.",
            pl_time(since_start, "%d.%m %H:%M")
        );
    } else {
        let _ = writeln!(
            full,
            "\nOd ostatniego startu ({}):\n\n| Potwierdzenie zlecenia | n | p50 ms | p90 ms | max ms |\n|---|---|---|---|---|",
            pl_time(since_start, "%d.%m %H:%M")
        );
        for (venue, xs) in &mut acks {
            let _ = writeln!(
                full,
                "| {venue} | {} | {:.0} | {:.0} | {:.0} |",
                xs.len(),
                pct(xs, 50.0),
                pct(xs, 90.0),
                pct(xs, 100.0)
            );
        }
        let _ = writeln!(
            short,
            "Potwierdzenia: {}",
            acks.iter_mut()
                .map(|(v, xs)| format!("{v} p50 {:.0} ms", pct(xs, 50.0)))
                .collect::<Vec<_>>()
                .join(", ")
        );
        for (venue, xs) in &mut acks {
            let slow = if venue == "okx" { 150.0 } else { 350.0 };
            if xs.len() >= 3 && pct(xs, 50.0) > slow {
                advice.push(format!(
                    "Zlecenia na {venue} potwierdzane wolno (mediana {:.0} ms): sprawdź łącze serwera albo rozważ serwer bliżej giełd.",
                    pct(xs, 50.0)
                ));
            }
        }
    }

    hedge_section(run, full, advice);

    let mut hours: BTreeMap<i64, (usize, f64)> = BTreeMap::new();
    for t in &trades {
        let h = pl_hour(ts(t));
        let e = hours.entry(h).or_default();
        e.0 += 1;
        e.1 += num(t, "pnl");
    }
    if !hours.is_empty() {
        let _ = writeln!(
            full,
            "\nGodziny sygnałów (czas polski): {}",
            hours
                .iter()
                .map(|(h, (n, p))| format!("{h:02}:00 {n}× {p:+.3}"))
                .collect::<Vec<_>>()
                .join(" | ")
        );
    }

    let skips: Vec<&Value> = run
        .of("skip")
        .into_iter()
        .filter(|s| text(s, "reason") == "no_funds")
        .collect();
    if !skips.is_empty() {
        let mut by_dir: BTreeMap<String, usize> = BTreeMap::new();
        for s in &skips {
            *by_dir
                .entry(format!(
                    "kup {} → sprzedaj {}",
                    text(s, "buy"),
                    text(s, "sell")
                ))
                .or_default() += 1;
        }
        let list = by_dir
            .iter()
            .map(|(d, n)| format!("{d}: {n}"))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(full, "\nPominięte z braku środków: {list}");
        advice.push(format!(
            "{} sygnałów pominiętych z braku środków ({list}): wyrównaj salda między giełdami.",
            skips.len()
        ));
    }

    let empty = trades.len() - done.len();
    if trades.len() >= 5 && empty * 100 >= trades.len() * 40 {
        advice.push(format!(
            "Na żywo {empty} z {} sygnałów bez wypełnienia: zapas ceny (--slack-bps) mógłby część z nich zamienić w transakcje.",
            trades.len()
        ));
    }
    if stall_min > 10.0 {
        advice.push(format!(
            "Komputer spał {stall_min:.0} min w oknie: bot w tym czasie nie handlował (serwer VPS rozwiązuje to na stałe)."
        ));
    }
    let _ = writeln!(full);
}

fn hedge_section(run: &Run, full: &mut String, advice: &mut Vec<String>) {
    let probed = |kind: &str| -> Vec<&Value> {
        run.of(kind)
            .into_iter()
            .filter(|v| v["book_age_ms"].is_number())
            .collect()
    };
    let (hedges, misses) = (probed("maker_hedge"), probed("hedge_miss"));
    if hedges.is_empty() && misses.is_empty() {
        return;
    }
    let column = |rows: &[&Value], key: &str| -> Vec<f64> {
        rows.iter().filter_map(|v| v[key].as_f64()).collect()
    };
    let slip = |v: &&Value| -> Option<f64> {
        let (top, avg) = (v["top"].as_f64()?, v["hedge_price"].as_f64()?);
        if v["flatten"].is_string() || top <= 0.0 {
            return None;
        }
        Some(match text(v, "side") {
            "buy" => (top - avg) / top * 1e4,
            _ => (avg - top) / top * 1e4,
        })
    };
    let flattened = hedges.iter().filter(|v| v["flatten"].is_string()).count();
    let _ = writeln!(
        full,
        "\nZabezpieczenia makera (próby na Bybit):\n\n| Próba | n | wiek książki Bybit p50/p90 ms | od wypełnienia p50/p90 ms | poślizg wobec ceny z książki śr. bp |\n|---|---|---|---|---|"
    );
    for (label, rows) in [("udana", &hedges), ("chybiona", &misses)] {
        let (mut age, mut wait) = (column(rows, "book_age_ms"), column(rows, "since_fill_ms"));
        let slips: Vec<f64> = rows.iter().filter_map(slip).collect();
        let _ = writeln!(
            full,
            "| {label} | {} | {:.0} / {:.0} | {:.0} / {:.0} | {} |",
            rows.len(),
            pct(&mut age, 50.0),
            pct(&mut age, 90.0),
            pct(&mut wait, 50.0),
            pct(&mut wait, 90.0),
            if slips.is_empty() {
                "-".to_owned()
            } else {
                format!("{:+.1}", mean(slips.iter().copied()))
            }
        );
    }
    if flattened > 0 {
        let _ = writeln!(
            full,
            "\nDomknięte na OKX po odrzuceniu zabezpieczenia na Bybit: {flattened}"
        );
    }
    let attempts = hedges.len() + misses.len();
    if misses.len() * 5 >= attempts && attempts >= MIN_CASES {
        let (mut ok_age, mut miss_age) = (
            column(&hedges, "book_age_ms"),
            column(&misses, "book_age_ms"),
        );
        let (ok_p50, miss_p50) = (pct(&mut ok_age, 50.0), pct(&mut miss_age, 50.0));
        advice.push(if miss_p50 > 2.0 * ok_p50.max(50.0) {
            format!(
                "Chybione zabezpieczenia ({} z {attempts}) mają starszą książkę Bybit (p50 {miss_p50:.0} ms wobec {ok_p50:.0} ms): dane z Bybit docierają z opóźnieniem.",
                misses.len()
            )
        } else {
            format!(
                "Chybione zabezpieczenia ({} z {attempts}) przy świeżej książce Bybit (p50 {miss_p50:.0} ms): cena ucieka, zanim zlecenie dojdzie — rozważ szerszy limit pierwszej próby.",
                misses.len()
            )
        });
    }
}

fn pair_since(prior: Option<&Value>, starts: &[&Value]) -> Option<f64> {
    let mut previous = prior.map(|p| &p["pair"]);
    let mut since = None;
    for s in starts {
        if previous.is_some_and(|p| *p != s["pair"]) {
            since = Some(ts(s));
        }
        previous = Some(&s["pair"]);
    }
    since
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hedge_misses_with_stale_books_are_reported() {
        let hedge = |age: f64| {
            json!({"type": "maker_hedge", "ts_ms": 1.0, "side": "buy", "top": 0.1500, "hedge_price": 0.1497,
                "book_age_ms": age, "since_fill_ms": 120.0, "pnl": 0.02})
        };
        let miss = |age: f64| json!({"type": "hedge_miss", "ts_ms": 1.0, "side": "buy", "book_age_ms": age, "since_fill_ms": 150.0});
        let mut rows: Vec<Value> = (0..6).map(|_| hedge(40.0)).collect();
        rows.extend((0..3).map(|_| miss(900.0)));
        let run = Run {
            label: "na żywo".to_owned(),
            live: true,
            start: None,
            prior_start: None,
            rows,
        };
        let (mut full, mut advice) = (String::new(), Vec::new());
        hedge_section(&run, &mut full, &mut advice);
        assert!(full.contains("| udana | 6 | 40 / 40 | 120 / 120 | +20.0 |"));
        assert!(full.contains("| chybiona | 3 | 900 / 900 | 150 / 150 | - |"));
        assert_eq!(advice.len(), 1);
        assert!(advice[0].contains("starszą książkę Bybit"));
    }

    #[test]
    fn capital_is_compared_from_the_last_pair_change() {
        let start = |t: f64, pair: &str| json!({"type": "start", "ts_ms": t, "pair": pair});
        let (tia, arb, arb2) = (
            start(10.0, "TIA/USDC"),
            start(20.0, "ARB/USDC"),
            start(30.0, "ARB/USDC"),
        );
        assert_eq!(pair_since(Some(&tia), &[&arb, &arb2]), Some(20.0));
        assert_eq!(pair_since(None, &[&tia, &arb, &arb2]), Some(20.0));
        assert_eq!(pair_since(Some(&arb), &[&arb2]), None);
        assert_eq!(pair_since(None, &[]), None);
    }

    #[test]
    fn latency_header_uses_the_last_start_before_the_window() {
        let run = Run {
            label: "na żywo".to_owned(),
            live: true,
            start: Some(
                json!({"type": "start", "ts_ms": 1_792_600_000_000.0_f64, "pair": "ARB/USDC"}),
            ),
            prior_start: None,
            rows: vec![
                json!({"type": "maker_hedge", "ts_ms": 1_792_800_000_000.0_f64, "pnl": 0.02,
                "orders": [{"venue": "bybit", "ack_ms": 190.0}]}),
            ],
        };
        let (mut full, mut short, mut advice) = (String::new(), String::new(), Vec::new());
        live_section(&run, &mut full, &mut short, &mut advice);
        assert!(full.contains(&format!(
            "Od ostatniego startu ({})",
            pl_time(1_792_600_000_000.0, "%d.%m %H:%M")
        )));
        assert!(!full.contains("01.01"));
    }
}
