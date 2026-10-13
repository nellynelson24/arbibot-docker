use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde_json::Value;

use super::stats::{mean, pct, pl_hour, pl_time};
use super::{Run, executed, num, text, ts};

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
    if let (Some(first), Some(last)) = (statuses.first(), statuses.last()) {
        let _ = writeln!(
            full,
            "- Kapitał widziany przez bota: {:.2} → {:.2} USDC (bez przelewów w drodze; różnica poza arbitrażem to kurs i przelewy)",
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
    let _ = writeln!(
        short,
        "\nNa żywo: {pnl:+.4} USDC, sygnały {}, wykonane {}, śr. {mean_bps:+.0} bp",
        trades.len(),
        done.len()
    );
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

    let since_start = run.of("start").last().map_or(0.0, |s| ts(s));
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
                    "Zlecenia na {venue} potwierdzane wolno (mediana {:.0} ms): sprawdź sieć (kabel zamiast Wi-Fi) albo rozważ serwer bliżej giełd.",
                    pct(xs, 50.0)
                ));
            }
        }
    }

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
            "Na żywo {empty} z {} sygnałów bez wypełnienia: porównaj warianty ab-slack10 i ab-35 — zapas ceny może je zamienić w transakcje.",
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
