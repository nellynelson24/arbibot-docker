use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde_json::Value;

use super::live::hedge_slip;
use super::stats::{mean, pct};
use super::{MIN_CASES, Run, SAME_SIGNAL_MS, executed, num, text, ts};

const MISSED_DUST: f64 = 1e-9;
const QUOTE_TIME: [&str; 5] = ["resting", "margin", "skew", "funds", "other"];
const FEWER_FILLS: f64 = 0.8;
const IDLE_SHARE: f64 = 0.1;

struct Breakdown {
    nominal: f64,
    threshold: f64,
    expected: f64,
    slip: f64,
    result: f64,
    fills: usize,
    hedges: usize,
    sim: Option<usize>,
    time: [f64; 5],
}

impl Breakdown {
    fn of(run: &Run, sim: Option<usize>) -> Option<Breakdown> {
        let fills: Vec<&Value> = run
            .of("maker_fill")
            .into_iter()
            .filter(|f| f["expected_bps"].is_number())
            .collect();
        if fills.is_empty() {
            return None;
        }
        let hedges = run.of("maker_hedge");
        Some(Breakdown {
            nominal: run.setting("maker_min_bps").unwrap_or(0.0),
            threshold: mean(fills.iter().map(|f| num(f, "threshold_bps"))),
            expected: mean(fills.iter().map(|f| num(f, "expected_bps"))),
            slip: mean(hedges.iter().filter_map(|h| hedge_slip(h))),
            result: mean(hedges.iter().map(|h| num(h, "pnl_bps"))),
            fills: fills.len(),
            hedges: hedges.len(),
            sim,
            time: quote_secs(&run.of("status")),
        })
    }

    fn share(&self, i: usize) -> f64 {
        let total: f64 = self.time.iter().sum();
        if total > 0.0 {
            self.time[i] / total
        } else {
            0.0
        }
    }

    fn losses(&self) -> [(&'static str, f64); 4] {
        [
            ("przechył (niższy próg)", self.nominal - self.threshold),
            (
                "wypełnienie poniżej progu (cena Bybit uciekła przed wypełnieniem)",
                self.threshold - self.expected,
            ),
            ("poślizg zabezpieczenia", self.slip),
            (
                "ruch ceny między wypełnieniem a zabezpieczeniem",
                self.expected - self.slip - self.result,
            ),
        ]
    }

    fn count_cause(&self) -> Option<String> {
        let sim = self.sim.filter(|n| *n > 0)?;
        let ratio = self.hedges as f64 / sim as f64;
        let (resting, skew, funds) = (self.share(0), self.share(2), self.share(3));
        let has_time = self.time.iter().sum::<f64>() > 0.0;
        Some(if ratio >= FEWER_FILLS {
            "liczba wypełnień zgodna z symulacją".to_owned()
        } else if !has_time {
            "brak danych o czasie kwotowania".to_owned()
        } else if funds >= IDLE_SHARE && funds >= skew {
            format!(
                "mniej wypełnień przez brak środków (bez zlecenia {:.0}% czasu)",
                funds * 100.0
            )
        } else if skew >= IDLE_SHARE {
            format!(
                "mniej wypełnień przez próg z przechyłem (bez zlecenia {:.0}% czasu)",
                skew * 100.0
            )
        } else {
            format!(
                "mniej wypełnień przez miejsce w kolejce albo wielkość zlecenia (zlecenie w książce {:.0}% czasu, a wypełnień mniej niż w symulacji)",
                resting * 100.0
            )
        })
    }

    fn advice(&self, label: &str) -> String {
        let losses = self.losses();
        let parts = losses
            .iter()
            .map(|(what, loss)| format!("{what} {:+.1}", -loss))
            .collect::<Vec<_>>()
            .join(", ");
        let biggest = losses
            .iter()
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map_or("", |l| l.0);
        let mut line = format!(
            "Maker ({label}): z {:.0} bp marży zostaje {:+.1} bp — {parts}; najwięcej zabiera {biggest}.",
            self.nominal, self.result
        );
        if let (Some(sim), Some(cause)) = (self.sim, self.count_cause()) {
            line += &format!(
                " Zabezpieczeń {} wobec {sim} wypełnień w symulacji skanera ({:.0}%): {cause}.",
                self.hedges,
                self.hedges as f64 / sim.max(1) as f64 * 100.0
            );
        }
        line
    }
}

fn counter_delta(xs: impl Iterator<Item = f64>) -> f64 {
    let (mut total, mut last) = (0.0, None);
    for x in xs {
        if let Some(l) = last {
            total += if x >= l { x - l } else { x };
        }
        last = Some(x);
    }
    total
}

fn quote_secs(statuses: &[&Value]) -> [f64; 5] {
    let mut out = [0.0; 5];
    for side in ["buy", "sell"] {
        for (i, id) in QUOTE_TIME.iter().enumerate() {
            out[i] += counter_delta(
                statuses
                    .iter()
                    .filter_map(|s| s["maker_secs"][side][id].as_f64()),
            );
        }
    }
    out
}

fn breakdown_section(
    runs: &[&Run],
    sim: &BTreeMap<String, usize>,
    full: &mut String,
    advice: &mut Vec<String>,
) {
    let rows: Vec<(&Run, Breakdown)> = runs
        .iter()
        .filter_map(|r| {
            let count = r.pair().map(|p| sim.get(&p).copied().unwrap_or(0));
            Breakdown::of(r, count).map(|b| (*r, b))
        })
        .collect();
    if rows.is_empty() {
        return;
    }
    let _ = writeln!(
        full,
        "## Maker: gdzie ginie marża i wypełnienia\n\nMarża w bp: próg = marża minus przechył, oczekiwana = wobec książki Bybit w chwili wypełnienia, poślizg = cena zabezpieczenia wobec najlepszej ceny Bybit w chwili zabezpieczenia, wynik = po zabezpieczeniu. Symulacja skanera: wypełnienia tej samej pary w tym oknie (zlecenia skanera są większe). Czas: udział sekund obu stron makera.\n\n| Wariant | marża | śr. próg | śr. oczekiwana | śr. poślizg | śr. wynik | zabezp. / symulacja | w książce | za mała marża | przechył | brak środków | inne |\n|---|---|---|---|---|---|---|---|---|---|---|---|"
    );
    for (run, b) in &rows {
        let pct = |i: usize| format!("{:.0}%", b.share(i) * 100.0);
        let _ = writeln!(
            full,
            "| {} | {:.0} | {:+.1} | {:+.1} | {:+.1} | {:+.1} | {} / {} | {} | {} | {} | {} | {} |",
            run.label,
            b.nominal,
            b.threshold,
            b.expected,
            b.slip,
            b.result,
            b.hedges,
            b.sim.map_or("-".to_owned(), |n| n.to_string()),
            pct(0),
            pct(1),
            pct(2),
            pct(3),
            pct(4)
        );
        if b.fills >= MIN_CASES {
            advice.push(b.advice(&run.label));
        }
    }
    let _ = writeln!(full);
}

pub(super) fn maker_section(
    runs: &[Run],
    sim: &BTreeMap<String, usize>,
    full: &mut String,
    advice: &mut Vec<String>,
) {
    let makers: Vec<&Run> = runs
        .iter()
        .filter(|r| !r.of("maker_hedge").is_empty())
        .collect();
    if makers.is_empty() {
        return;
    }
    let _ = writeln!(
        full,
        "## Maker na OKX\n\n| Wariant | wypełnienia | zabezpieczenia | suma USDC | śr. bp | mediana bp | na plus | kupno USDC | sprzedaż USDC | niepełne zabezp. |\n|---|---|---|---|---|---|---|---|---|---|"
    );
    for run in &makers {
        let hedges = run.of("maker_hedge");
        let usdc = run.usdc();
        let mut bps: Vec<f64> = hedges.iter().map(|h| num(h, "pnl_bps")).collect();
        let side = |s: &str| -> f64 {
            hedges
                .iter()
                .filter(|h| text(h, "side") == s)
                .map(|h| num(h, "pnl"))
                .fold(0.0, |a, b| a + b)
                * usdc
        };
        let positive = hedges.iter().filter(|h| num(h, "pnl") > 0.0).count();
        let mean_bps = mean(bps.iter().copied());
        let _ = writeln!(
            full,
            "| {} | {} | {} | {:+.4} | {mean_bps:+.1} | {:+.1} | {positive}/{} | {:+.4} | {:+.4} | {} |",
            run.label,
            run.of("maker_fill").len(),
            hedges.len(),
            hedges.iter().map(|h| num(h, "pnl")).fold(0.0, |a, b| a + b) * usdc,
            pct(&mut bps, 50.0),
            hedges.len(),
            side("buy"),
            side("sell"),
            hedges
                .iter()
                .filter(|h| num(h, "missed") > MISSED_DUST)
                .count()
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
    breakdown_section(&makers, sim, full, advice);
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(rows: Vec<Value>) -> Run {
        Run {
            label: "główny".to_owned(),
            live: false,
            start: Some(json!({"type": "start", "pair": "ARB/USDC", "maker_min_bps": 20.0})),
            prior_start: None,
            rows,
        }
    }

    fn secs(resting: f64, margin: f64, funds: f64) -> Value {
        json!({"resting": resting, "margin": margin, "skew": 0.0, "funds": funds, "other": 0.0})
    }

    #[test]
    fn breakdown_says_where_the_margin_and_the_fills_go() {
        let mut rows = Vec::new();
        for _ in 0..10 {
            rows.push(json!({"type": "maker_fill", "side": "buy", "expected_bps": 14.0, "threshold_bps": 10.0}));
            rows.push(json!({"type": "maker_hedge", "side": "buy", "pnl_bps": 10.0, "top": 1.0, "hedge_price": 0.9999}));
        }
        rows.push(json!({"type": "maker_fill", "side": "buy"}));
        rows.push(json!({"type": "status", "maker_secs": {"buy": secs(0.0, 0.0, 0.0), "sell": secs(0.0, 0.0, 0.0)}}));
        rows.push(json!({"type": "status", "maker_secs": {"buy": secs(100.0, 0.0, 300.0), "sell": secs(100.0, 500.0, 0.0)}}));
        let b = Breakdown::of(&run(rows), Some(20)).unwrap();
        assert_eq!(b.fills, 10);
        assert!((b.slip - 1.0).abs() < 1e-6);
        assert!((b.share(3) - 0.3).abs() < 1e-9);
        let text = b.advice("główny");
        assert!(
            text.starts_with("Maker (główny): z 20 bp marży zostaje +10.0 bp"),
            "{text}"
        );
        assert!(text.contains("przechył (niższy próg) -10.0"), "{text}");
        assert!(
            text.contains("wypełnienie poniżej progu (cena Bybit uciekła przed wypełnieniem) +4.0"),
            "{text}"
        );
        assert!(text.contains("najwięcej zabiera przechył"), "{text}");
        assert!(text.contains("Zabezpieczeń 10 wobec 20 wypełnień w symulacji skanera (50%): mniej wypełnień przez brak środków (bez zlecenia 30% czasu)"), "{text}");
    }

    #[test]
    fn fewer_fills_with_the_order_resting_point_to_the_queue() {
        let rows = vec![
            json!({"type": "maker_fill", "side": "sell", "expected_bps": 20.0, "threshold_bps": 20.0}),
            json!({"type": "maker_hedge", "side": "sell", "pnl_bps": 19.0, "top": 1.0, "hedge_price": 1.0}),
            json!({"type": "status", "maker_secs": {"buy": secs(50.0, 0.0, 0.0), "sell": secs(50.0, 0.0, 0.0)}}),
            json!({"type": "status", "maker_secs": {"buy": secs(20.0, 0.0, 0.0), "sell": secs(10.0, 5.0, 0.0)}}),
        ];
        let b = Breakdown::of(&run(rows), Some(10)).unwrap();
        assert_eq!(b.time, [30.0, 5.0, 0.0, 0.0, 0.0]);
        assert!(b.count_cause().unwrap().contains("miejsce w kolejce"));
        assert!(
            b.advice("główny")
                .contains("najwięcej zabiera ruch ceny między wypełnieniem a zabezpieczeniem")
        );
        assert!(Breakdown::of(&run(vec![json!({"type": "maker_fill"})]), None).is_none());
    }
}
