use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use serde_json::Value;
use venues::Book;

use super::stats::mean;
use super::{MIN_CASES, Run, num, text, ts};
use crate::selector::{liquidity_issue_on, utc_date};

const DAY_MS: f64 = 86_400_000.0;
const HOUR_MS: f64 = 3_600_000.0;
pub(super) const RANK_DAYS: f64 = 14.0;
const SHOWN: usize = 8;
const SWITCH_COST: f64 = 0.4;
const PAYBACK_DAYS: f64 = 7.0;
const LIVE_HEDGE: &str = "bybit";
const HEDGES: [(usize, &str, &str); 2] = [(1, "bybit", "Bybit"), (2, "bitvavo", "Bitvavo")];
const FULL_DAY_HOURS: usize = 23;
const LOW_COVERAGE: f64 = 0.8;

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

struct SimFill {
    ts: f64,
    hedge: String,
    pair: String,
    bps: Option<f64>,
}

pub(super) struct Scan {
    fills: Vec<SimFill>,
    times: Vec<f64>,
}

type Range = (f64, f64);

impl Scan {
    pub(super) fn read(data_dir: &Path, (since, until): Range) -> Scan {
        let mut scan = Scan {
            fills: Vec::new(),
            times: Vec::new(),
        };
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
                scan.times.push(t);
                let hedge = text(&v, "hedge");
                if v["type"] != "fill"
                    || text(&v, "maker") != "okx"
                    || !HEDGES.iter().any(|(_, id, _)| *id == hedge)
                {
                    continue;
                }
                scan.fills.push(SimFill {
                    ts: t,
                    hedge: hedge.to_owned(),
                    pair: text(&v, "pair").to_owned(),
                    bps: v["edge_bps"].as_f64(),
                });
            }
        }
        scan
    }

    fn pairs(&self, hedge: &str, (since, until): Range) -> BTreeMap<String, PairFills> {
        let mut out: BTreeMap<String, PairFills> = BTreeMap::new();
        for f in self
            .fills
            .iter()
            .filter(|f| f.hedge == hedge && f.ts >= since && f.ts <= until)
        {
            let p = out.entry(f.pair.clone()).or_default();
            p.fills += 1;
            match f.bps {
                Some(bps) => {
                    p.positive += usize::from(bps > 0.0);
                    p.bps.push(bps);
                }
                None => p.unhedged += 1,
            }
        }
        out
    }

    pub(super) fn counts(&self, (since, until): Range) -> BTreeMap<String, usize> {
        self.pairs(LIVE_HEDGE, (since, until))
            .into_iter()
            .map(|(p, f)| (p, f.fills))
            .collect()
    }

    fn hours(&self, (since, until): Range) -> usize {
        self.times
            .iter()
            .filter(|t| **t >= since && **t < until)
            .map(|t| ((t - since) / HOUR_MS).floor() as i64)
            .collect::<BTreeSet<_>>()
            .len()
    }

    fn days(&self, range: Range) -> f64 {
        (self.hours(range) as f64 / 24.0).max(1.0 / 24.0)
    }

    fn short_days(&self, (since, until): Range) -> Vec<(f64, usize)> {
        let mut day = (since / DAY_MS).ceil() * DAY_MS;
        let mut out = Vec::new();
        while day + DAY_MS <= until {
            let hours = self.hours((day, day + DAY_MS));
            if hours < FULL_DAY_HOURS {
                out.push((day, hours));
            }
            day += DAY_MS;
        }
        out
    }
}

pub(super) fn rank_range((since, until): Range) -> Range {
    (since.min(until - RANK_DAYS * DAY_MS), until)
}

fn reason_kind(reason: &str) -> String {
    reason
        .split(' ')
        .take_while(|w| !w.chars().any(|c| c.is_ascii_digit()))
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn scanner_maker_section(
    (scan, books): (&Scan, Option<&[Book; 3]>),
    run: Option<&Run>,
    (since, until): Range,
    full: &mut String,
    advice: &mut Vec<String>,
) {
    let pair = run.and_then(Run::pair).unwrap_or_default();
    let size = run
        .and_then(|r| r.start.as_ref())
        .and_then(|s| s["maker_size"].as_f64())
        .unwrap_or(10.0);
    let window = (since, until);
    let long = (until - RANK_DAYS * DAY_MS, until);
    let halves = [
        (long.0, long.0 + RANK_DAYS / 2.0 * DAY_MS),
        (long.0 + RANK_DAYS / 2.0 * DAY_MS, until),
    ];
    let (window_days, long_days) = (scan.days(window), scan.days(long));
    let (window_hours, long_hours) = (scan.hours(window), scan.hours(long));
    let span_hours = ((until - since) / HOUR_MS).ceil() as usize;
    let short_days = scan.short_days(long);

    for (index, id, name) in HEDGES {
        let long_fills = scan.pairs(id, long);
        if long_fills.is_empty() {
            continue;
        }
        let window_fills = scan.pairs(id, window);
        let half_fills = halves.map(|h| (scan.pairs(id, h), scan.days(h)));
        let ours = |p: &str| id == LIVE_HEDGE && p == pair;
        let empty = PairFills::default();
        let on = |fills: &BTreeMap<String, PairFills>, p: &str, days: f64| {
            fills.get(p).unwrap_or(&empty).per_day(size, days)
        };
        let mut dropped: BTreeMap<String, usize> = BTreeMap::new();
        let mut ranked: Vec<(&String, &PairFills)> = Vec::new();
        for (p, f) in &long_fills {
            let issue =
                books.and_then(|b| liquidity_issue_on(p, [&b[0], &b[index]], ["OKX", name]));
            match issue {
                Some(reason) if !ours(p) => *dropped.entry(reason_kind(&reason)).or_default() += 1,
                _ => ranked.push((p, f)),
            }
        }
        ranked.sort_by(|a, b| {
            b.1.per_day(size, long_days)
                .total_cmp(&a.1.per_day(size, long_days))
        });

        let _ = writeln!(
            full,
            "## Maker według skanera (zlecenie na OKX, zabezpieczenie na {name}, symulacja)\n\nSzacunek przy zleceniu {size} w walucie kwotowania. Ranking z {RANK_DAYS:.0} dni ({long_hours} h danych skanera), obok okno raportu ({window_hours} z {span_hours} h danych skanera){}.\n",
            if books.is_some() {
                ", tylko pary po filtrach płynności"
            } else {
                ", bez filtrów płynności (brak notowań)"
            }
        );
        if !short_days.is_empty() {
            let _ = writeln!(
                full,
                "Dni z niepełnymi danymi skanera (UTC, godziny z danymi): {}\n",
                short_days
                    .iter()
                    .map(|(d, h)| format!("{} {h} h", utc_date(*d)))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let _ = writeln!(
            full,
            "Pary z wypełnieniami w symulacji ({RANK_DAYS:.0} dni): {}, po filtrach płynności: {}{}.\n",
            long_fills.len(),
            ranked.len(),
            if dropped.is_empty() {
                String::new()
            } else {
                format!(
                    "; odpadły: {}",
                    dropped
                        .iter()
                        .map(|(r, n)| format!("{r} {n}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        );
        if books.is_some_and(|b| b[index].is_empty()) {
            let _ = writeln!(
                full,
                "Notowania {name} były niedostępne w chwili raportu, więc żadna para nie przeszła filtrów.\n"
            );
        }
        if ranked.is_empty() {
            continue;
        }
        let _ = writeln!(
            full,
            "| Para | {RANK_DAYS:.0} dni: wypełnienia | śr. bp | zysk/dzień | okno: wypełnienia | śr. bp | zysk/dzień | bez zabezp. |\n|---|---|---|---|---|---|---|---|"
        );
        let rows = ranked
            .iter()
            .enumerate()
            .filter(|(i, (p, _))| *i < SHOWN || ours(p));
        for (_, (p, f)) in rows {
            let mark = if ours(p) { " (obecna)" } else { "" };
            let w = window_fills.get(*p).unwrap_or(&empty);
            let _ = writeln!(
                full,
                "| {p}{mark} | {} | {:+.1} | {:+.3} | {} | {:+.1} | {:+.3} | {} |",
                f.fills,
                mean(f.bps.iter().copied()),
                f.per_day(size, long_days),
                w.fills,
                mean(w.bps.iter().copied()),
                w.per_day(size, window_days),
                f.unhedged
            );
        }
        let _ = writeln!(full);

        if let (Some(run), Some(sim)) = (run, window_fills.get(&pair).filter(|_| id == LIVE_HEDGE))
        {
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

        let current = on(&long_fills, &pair, long_days);
        let current_window = on(&window_fills, &pair, window_days);
        let leader = ranked.first().filter(|(best, f)| {
            !ours(best)
                && f.bps.len() >= MIN_CASES
                && (f.per_day(size, long_days) - current) * PAYBACK_DAYS >= SWITCH_COST
                && half_fills
                    .iter()
                    .all(|(h, days)| on(h, best, *days) > on(h, &pair, *days))
        });
        if let Some((best, f)) = leader {
            let best_day = f.per_day(size, long_days);
            let best_window = on(&window_fills, best, window_days);
            advice.push(if id == LIVE_HEDGE {
                format!(
                    "Skaner (maker OKX → {name}): {best} ~{best_day:+.3} USDC/dzień w {RANK_DAYS:.0} dniach wobec {current:+.3} dla {pair}, lepsza w obu tygodniach (w oknie {best_window:+.3} wobec {current_window:+.3}); zmiana pary (~{SWITCH_COST} USDC) zwróci się w tydzień. Sprawdź go najpierw na sucho."
                )
            } else {
                format!(
                    "Skaner (maker OKX → {name}): {best} ~{best_day:+.3}/dzień w {RANK_DAYS:.0} dniach wobec {current:+.3} dla {pair} z zabezpieczeniem na Bybit, lepsza w obu tygodniach. Handel na {name} wymaga konta i modułu tej giełdy w bocie."
                )
            });
        }
        let window_leader = ranked
            .iter()
            .filter(|(p, f)| !ours(p) && f.bps.len() >= MIN_CASES)
            .max_by(|a, b| {
                on(&window_fills, a.0, window_days).total_cmp(&on(&window_fills, b.0, window_days))
            });
        if let Some((w, f)) = window_leader
            && leader.is_none_or(|(best, _)| best != w)
            && (on(&window_fills, w, window_days) - current_window) * PAYBACK_DAYS >= SWITCH_COST
        {
            let _ = writeln!(
                full,
                "W oknie raportu najlepsza {w} ({:+.3}/dzień wobec {current_window:+.3} dla {pair}), ale w {RANK_DAYS:.0} dniach ma {:+.3} wobec {current:+.3} albo nie wygrywa w obu tygodniach — bez rady o zmianie pary.\n",
                on(&window_fills, w, window_days),
                f.per_day(size, long_days)
            );
        }
    }
    if span_hours > 0 && (window_hours as f64) < span_hours as f64 * LOW_COVERAGE {
        advice.push(format!(
            "Skaner ma dane z {window_hours} z {span_hours} h okna: jego wyniki w oknie są niepełne (sprawdź kontener scanner)."
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use venues::Quote;

    const NOW: f64 = 1_792_900_800_000.0;

    struct Fixture {
        dir: std::path::PathBuf,
        lines: BTreeMap<String, Vec<String>>,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("arbibot-{name}-{}", std::process::id()));
            fs::create_dir_all(dir.join("maker")).unwrap();
            Fixture {
                dir,
                lines: BTreeMap::new(),
            }
        }

        fn row(&mut self, v: Value) {
            let day = utc_date(ts(&v));
            self.lines.entry(day).or_default().push(v.to_string());
        }

        fn hourly(&mut self, skip: impl Fn(f64) -> bool) {
            let mut t = NOW - RANK_DAYS * DAY_MS + 1_000.0;
            while t < NOW {
                if !skip(t) {
                    self.row(json!({"type": "status", "ts_ms": t}));
                }
                t += HOUR_MS;
            }
        }

        fn fills(
            &mut self,
            pair: &str,
            hedge: &str,
            (from, to): Range,
            n: usize,
            bps: Option<f64>,
        ) {
            for k in 0..n {
                let t = from + (to - from) * (k as f64 + 0.5) / n as f64;
                self.row(
                    json!({"type": "fill", "ts_ms": t, "pair": pair, "maker": "okx",
                    "hedge": hedge, "edge_bps": bps}),
                );
            }
        }

        fn report(
            &self,
            run: &Run,
            books: Option<&[Book; 3]>,
            hours: f64,
        ) -> (String, Vec<String>) {
            for (day, lines) in &self.lines {
                fs::write(
                    self.dir.join("maker").join(format!("{day}.jsonl")),
                    lines.join("\n"),
                )
                .unwrap();
            }
            let window = (NOW - hours * HOUR_MS, NOW);
            let scan = Scan::read(&self.dir, rank_range(window));
            let (mut full, mut advice) = (String::new(), Vec::new());
            scanner_maker_section((&scan, books), Some(run), window, &mut full, &mut advice);
            fs::remove_dir_all(&self.dir).unwrap();
            (full, advice)
        }
    }

    fn run(pair: &str) -> Run {
        Run {
            label: "na żywo".to_owned(),
            live: true,
            start: Some(json!({"type": "start", "pair": pair, "maker_size": 10.0})),
            prior_start: None,
            rows: vec![json!({"type": "start", "ts_ms": NOW - DAY_MS, "pair": pair})],
        }
    }

    fn books(pairs: &[(&str, &str, f64)], bitvavo: bool) -> [Book; 3] {
        let quote = |volume: f64| Quote {
            bid: 1.0,
            bid_qty: None,
            ask: 1.0001,
            ask_qty: None,
            volume: Some(volume),
        };
        let mut books = [Book::new(), Book::new(), Book::new()];
        for (base, q, volume) in pairs {
            for (i, b) in books.iter_mut().enumerate() {
                if i < 2 || bitvavo {
                    b.insert((base.to_string(), q.to_string()), quote(*volume));
                }
            }
        }
        books
    }

    const LONG: Range = (NOW - RANK_DAYS * DAY_MS, NOW);
    const FIRST_WEEK: Range = (NOW - RANK_DAYS * DAY_MS, NOW - 7.0 * DAY_MS);
    const SECOND_WEEK: Range = (NOW - 7.0 * DAY_MS, NOW);

    #[test]
    fn ranks_liquid_pairs_by_fourteen_days_of_simulated_maker_profit() {
        let mut f = Fixture::new("makers-rank");
        f.hourly(|_| false);
        for week in [FIRST_WEEK, SECOND_WEEK] {
            f.fills("ARB/USDC", "bybit", week, 10, Some(20.0));
            f.fills("WLD/USDC", "bybit", week, 100, Some(60.0));
            f.fills("ANIME/USDC", "bybit", week, 10, Some(200.0));
            f.fills("BTC/USDC", "bitvavo", week, 100, Some(60.0));
        }
        f.fills("WLD/USDC", "bybit", LONG, 1, None);
        f.row(
            json!({"type": "fill", "ts_ms": NOW - 1_000.0, "pair": "WLD/USDC", "maker": "kraken",
            "hedge": "bybit", "edge_bps": 500.0}),
        );
        let books = books(
            &[
                ("ARB", "USDC", 1e6),
                ("WLD", "USDC", 1e6),
                ("ANIME", "USDC", 500.0),
                ("BTC", "USDC", 1e6),
            ],
            true,
        );
        let (full, advice) = f.report(&run("ARB/USDC"), Some(&books), 24.0);

        let wld = full.find("| WLD/USDC |").unwrap();
        let arb = full.find("| ARB/USDC (obecna) |").unwrap();
        assert!(wld < arb);
        assert!(!full.contains("| ANIME"));
        assert!(full.contains("Ranking z 14 dni (336 h danych skanera), obok okno raportu (24 z 24 h danych skanera)"), "{full}");
        assert!(
            full.contains("| WLD/USDC | 201 | +60.0 | +0.857 |"),
            "{full}"
        );
        assert!(
            full.contains("| ARB/USDC (obecna) | 20 | +20.0 | +0.029 |"),
            "{full}"
        );
        assert!(full.contains("Pary z wypełnieniami w symulacji (14 dni): 3, po filtrach płynności: 2; odpadły: obrót OKX 1."), "{full}");
        assert!(
            full.contains("| BTC/USDC | 200 | +60.0 | +0.857 |"),
            "{full}"
        );
        assert_eq!(advice.len(), 2, "{advice:?}");
        assert!(advice[0].starts_with(
            "Skaner (maker OKX → Bybit): WLD/USDC ~+0.857 USDC/dzień w 14 dniach wobec +0.029"
        ));
        assert!(advice[1].starts_with("Skaner (maker OKX → Bitvavo): BTC/USDC ~+0.857/dzień"));
    }

    #[test]
    fn a_pair_better_for_two_days_but_worse_over_fourteen_gets_no_advice() {
        let mut f = Fixture::new("makers-recent");
        let gap = (NOW - 30.0 * HOUR_MS, NOW - 27.0 * HOUR_MS);
        f.hourly(|t| t >= gap.0 && t < gap.1);
        f.fills(
            "ARB/USDC",
            "bybit",
            (LONG.0, NOW - 2.0 * DAY_MS),
            120,
            Some(25.0),
        );
        f.fills(
            "ARB/USDC",
            "bybit",
            (NOW - 2.0 * DAY_MS, gap.0 - 1.0),
            5,
            Some(25.0),
        );
        f.fills("PEPE/USDC", "bybit", (gap.1, NOW), 40, Some(25.0));
        let books = books(&[("ARB", "USDC", 1e6), ("PEPE", "USDC", 1e6)], true);
        let (full, advice) = f.report(&run("ARB/USDC"), Some(&books), 48.0);

        assert!(advice.is_empty(), "{advice:?}");
        assert!(full.contains("(45 z 48 h danych skanera)"), "{full}");
        assert!(
            full.contains("W oknie raportu najlepsza PEPE/USDC"),
            "{full}"
        );
        assert!(full.contains("bez rady o zmianie pary"), "{full}");
        assert!(full.find("| ARB/USDC (obecna) |").unwrap() < full.find("| PEPE/USDC |").unwrap());
    }

    #[test]
    fn report_says_why_bitvavo_pairs_dropped_and_when_scanner_data_is_missing() {
        let mut f = Fixture::new("makers-bitvavo");
        let lost_day = (NOW / DAY_MS).floor() * DAY_MS - 9.0 * DAY_MS;
        f.hourly(|t| t >= lost_day && t < lost_day + 21.0 * HOUR_MS);
        f.fills("ARB/USDC", "bybit", (NOW - DAY_MS, NOW), 10, Some(20.0));
        f.fills("BTC/EUR", "bitvavo", SECOND_WEEK, 30, Some(30.0));
        f.fills("ETH/EUR", "bitvavo", SECOND_WEEK, 30, Some(30.0));
        let books = books(
            &[
                ("ARB", "USDC", 1e6),
                ("BTC", "EUR", 1e6),
                ("ETH", "EUR", 1e6),
                ("USDC", "EUR", 1e6),
            ],
            false,
        );
        let (full, advice) = f.report(&run("ARB/USDC"), Some(&books), 24.0);

        assert!(full.contains("Pary z wypełnieniami w symulacji (14 dni): 2, po filtrach płynności: 0; odpadły: brak notowań na Bitvavo 2."), "{full}");
        assert!(
            full.contains("Notowania Bitvavo były niedostępne w chwili raportu"),
            "{full}"
        );
        assert!(
            full.contains(&format!(
                "Dni z niepełnymi danymi skanera (UTC, godziny z danymi): {} 3 h",
                utc_date(lost_day)
            )),
            "{full}"
        );
        assert!(advice.is_empty(), "{advice:?}");
        assert_eq!(reason_kind("obrót Bitvavo 12410 USD"), "obrót Bitvavo");
        assert_eq!(reason_kind("różnica cen 150 bp"), "różnica cen");
        assert_eq!(
            reason_kind("brak notowań na Bitvavo"),
            "brak notowań na Bitvavo"
        );
    }

    #[test]
    fn a_short_window_of_scanner_data_is_flagged() {
        let mut f = Fixture::new("makers-coverage");
        f.hourly(|t| t > NOW - 20.0 * HOUR_MS);
        f.fills("ARB/USDC", "bybit", FIRST_WEEK, 20, Some(20.0));
        let (_, advice) = f.report(&run("ARB/USDC"), None, 24.0);
        assert!(
            advice
                .iter()
                .any(|a| a.starts_with("Skaner ma dane z 4 z 24 h okna")),
            "{advice:?}"
        );
    }
}
