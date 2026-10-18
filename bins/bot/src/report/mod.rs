mod compare;
mod live;
mod market;
mod stats;
mod strategy;

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::DateTime;
use serde_json::Value;
use tokio::time::sleep;

use crate::ledger::now_ms;
use crate::telegram::Telegram;
use compare::{agreement_section, variants_section};
use live::live_section;
use market::market_section;
use stats::{pl_hour, pl_time};
use strategy::{calibration_section, maker_section};

const HOUR_MS: f64 = 3_600_000.0;
const SAME_SIGNAL_MS: f64 = 5_000.0;
const MIN_CASES: usize = 8;
const TELEGRAM_LIMIT: usize = 3_900;
const DAILY_HOUR_PL: i64 = 8;
const DAILY_CHECK: Duration = Duration::from_secs(30);

#[derive(clap::Args)]
pub struct Args {
    #[arg(long, default_value_t = 24.0)]
    #[arg(help = "Okno raportu w godzinach, wstecz od teraz")]
    hours: f64,
    #[arg(long, default_value = "data")]
    #[arg(help = "Katalog danych skanera i botów")]
    data_dir: PathBuf,
    #[arg(long)]
    #[arg(help = "Wyślij skrót raportu na Telegram (Tobie i widzom)")]
    telegram: bool,
    #[arg(long)]
    #[arg(help = "Działaj stale i rób raport codziennie o 8:00 czasu polskiego")]
    daily: bool,
}

pub struct Report {
    pub full: String,
    pub short: String,
}

pub async fn run(args: Args) -> Result<()> {
    if args.daily {
        return daily(args).await;
    }
    let until = now_ms();
    let report = build(&args.data_dir, until - args.hours * HOUR_MS, until)?;
    let path = save(&args.data_dir, &report, until)?;
    println!("{}\nZapisano: {}", report.full, path.display());
    if args.telegram {
        Telegram::load()?.send_report(&report.short).await;
    }
    Ok(())
}

async fn daily(args: Args) -> Result<()> {
    let telegram = match args.telegram {
        true => Some(Telegram::load()?),
        false => None,
    };
    println!("Raport codziennie o {DAILY_HOUR_PL}:00 czasu polskiego.");
    loop {
        let now = now_ms();
        if let Some(marker) = daily_due(&args.data_dir, now) {
            match build(&args.data_dir, now - args.hours * HOUR_MS, now) {
                Ok(report) => {
                    let path = save(&args.data_dir, &report, now)?;
                    if let Some(telegram) = &telegram {
                        telegram.send_report(&report.short).await;
                    }
                    println!("{} raport: {}", pl_time(now, "%d.%m %H:%M"), path.display());
                }
                Err(e) => println!("{} raport nieudany: {e:#}", pl_time(now, "%d.%m %H:%M")),
            }
            fs::write(&marker, "").with_context(|| format!("zapis {}", marker.display()))?;
        }
        sleep(DAILY_CHECK).await;
    }
}

pub fn daily_due(data_dir: &Path, now: f64) -> Option<PathBuf> {
    if pl_hour(now) != DAILY_HOUR_PL {
        return None;
    }
    let dir = data_dir.join("reports");
    fs::create_dir_all(&dir).ok()?;
    let marker = dir.join(format!(".telegram-{}", pl_time(now, "%Y-%m-%d")));
    (!marker.exists()).then_some(marker)
}

pub fn save(data_dir: &Path, report: &Report, until: f64) -> Result<PathBuf> {
    let dir = data_dir.join("reports");
    fs::create_dir_all(&dir).with_context(|| format!("katalog {}", dir.display()))?;
    let stamp = DateTime::from_timestamp_millis(until as i64)
        .map(|t| t.format("%Y-%m-%d_%H%M").to_string())
        .unwrap_or_default();
    let path = dir.join(format!("{stamp}.md"));
    fs::write(&path, &report.full).with_context(|| format!("zapis {}", path.display()))?;
    Ok(path)
}

struct Run {
    label: String,
    live: bool,
    start: Option<Value>,
    rows: Vec<Value>,
}

impl Run {
    fn of(&self, kind: &str) -> Vec<&Value> {
        self.rows.iter().filter(|v| v["type"] == kind).collect()
    }

    fn trades(&self) -> Vec<&Value> {
        self.of("trade")
    }

    fn sum(&self, kind: &str, key: &str) -> f64 {
        self.of(kind)
            .iter()
            .map(|v| num(v, key))
            .fold(0.0, |a, b| a + b)
    }

    fn rebalance_cost(&self) -> f64 {
        self.sum("rebalance", "cost") - self.sum("rebalance_trade", "pnl")
    }

    fn total(&self) -> f64 {
        self.sum("trade", "pnl") + self.sum("maker_hedge", "pnl") - self.rebalance_cost()
    }

    fn setting(&self, key: &str) -> Option<f64> {
        self.start.as_ref().and_then(|s| s[key].as_f64())
    }

    fn pair(&self) -> Option<String> {
        self.start
            .as_ref()
            .and_then(|s| s["pair"].as_str())
            .map(str::to_owned)
    }

    fn settings(&self) -> String {
        let Some(s) = &self.start else {
            return "-".to_owned();
        };
        let mut parts = vec![
            s["pair"].as_str().unwrap_or("?").to_owned(),
            format!("próg {}", num(s, "min_bps")),
            format!("{} USD", num(s, "size")),
        ];
        if num(s, "slack_bps") > 0.0 {
            parts.push(format!("zapas {}", num(s, "slack_bps")));
        }
        if s["maker"] == true {
            parts.push(format!("maker {}", num(s, "maker_min_bps")));
        }
        if let Some(latency) = s["latency_ms"].as_array()
            && !self.live
        {
            parts.push(format!(
                "opóźn. {}/{}",
                latency.first().and_then(Value::as_f64).unwrap_or(0.0),
                latency.get(1).and_then(Value::as_f64).unwrap_or(0.0)
            ));
        }
        parts.join(", ")
    }
}

fn book_gaps(statuses: &[&Value]) -> u64 {
    let counts =
        |s: &Value| -> [u64; 2] { [0, 1].map(|v| s["book_gaps"][v].as_u64().unwrap_or(0)) };
    let mut total = 0;
    let mut last = statuses.first().map_or([0, 0], |s| counts(s));
    for s in statuses.iter().skip(1) {
        let now = counts(s);
        for v in 0..2 {
            total += if now[v] >= last[v] {
                now[v] - last[v]
            } else {
                now[v]
            };
        }
        last = now;
    }
    total
}

fn num(v: &Value, key: &str) -> f64 {
    v[key].as_f64().unwrap_or(0.0)
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or_default()
}

fn ts(v: &Value) -> f64 {
    num(v, "ts_ms")
}

fn executed(trade: &Value) -> bool {
    text(trade, "outcome") != "brak wypełnienia"
}

pub fn build(data_dir: &Path, since: f64, until: f64) -> Result<Report> {
    let runs: Vec<Run> = sources(data_dir)
        .into_iter()
        .filter_map(|(label, dir, live)| load(label, &dir, live, since, until))
        .collect();
    let live = runs.iter().find(|r| r.live);
    let mut full = String::new();
    let mut short = String::new();
    let mut advice = Vec::new();

    let _ = writeln!(
        full,
        "# Raport federbot: {} → {} (czas polski, {:.0} h)\n",
        pl_time(since, "%d.%m %H:%M"),
        pl_time(until, "%d.%m %H:%M"),
        (until - since) / HOUR_MS
    );
    let _ = writeln!(
        short,
        "Raport {:.0} h do {}",
        (until - since) / HOUR_MS,
        pl_time(until, "%d.%m %H:%M")
    );

    if let Some(live) = live {
        live_section(live, &mut full, &mut short, &mut advice);
    } else {
        let _ = writeln!(full, "## Na żywo\n\nBrak handlu na żywo w tym oknie.\n");
    }
    variants_section(&runs, &mut full, &mut short, &mut advice);
    if let Some(live) = live {
        agreement_section(live, &runs, &mut full);
    }
    maker_section(&runs, &mut full, &mut advice);
    calibration_section(&runs, &mut full, &mut advice);
    if let Some(run) = live.or(runs.first()) {
        let gaps = book_gaps(&run.of("status"));
        let _ = writeln!(
            full,
            "## Książki zleceń\n\n- Luki i skrzyżowania ({}): {gaps}\n",
            run.label
        );
        if gaps > 0 {
            let _ = writeln!(short, "\nLuki w książkach ({}): {gaps}", run.label);
        }
    }
    market_section(
        data_dir,
        live.or(runs.first()),
        &runs,
        (since, until),
        &mut full,
        &mut short,
        &mut advice,
    );

    let _ = writeln!(full, "## Wnioski\n");
    if advice.is_empty() {
        let _ = writeln!(full, "- Za mało danych na wnioski w tym oknie.");
    }
    for a in &advice {
        let _ = writeln!(full, "- {a}");
    }
    let _ = writeln!(short, "\nWnioski:");
    for a in advice.iter().take(6) {
        let _ = writeln!(short, "• {a}");
    }
    if advice.is_empty() {
        let _ = writeln!(short, "• za mało danych");
    }
    if short.chars().count() > TELEGRAM_LIMIT {
        short = short.chars().take(TELEGRAM_LIMIT).collect::<String>() + "…";
    }
    Ok(Report { full, short })
}

fn sources(data_dir: &Path) -> Vec<(String, PathBuf, bool)> {
    let mut out = vec![
        ("na żywo".to_owned(), data_dir.join("live"), true),
        ("główny".to_owned(), data_dir.join("bot"), false),
    ];
    let mut ab: Vec<(String, PathBuf, bool)> = fs::read_dir(data_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with("ab-")
                .then(|| (name, e.path().join("bot"), false))
        })
        .collect();
    ab.sort_by(|a, b| a.0.cmp(&b.0));
    out.extend(ab);
    out
}

fn load(label: String, dir: &Path, live: bool, since: f64, until: f64) -> Option<Run> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .collect();
    files.sort();
    let mut start = None;
    let mut rows = Vec::new();
    for file in files {
        let Ok(content) = fs::read_to_string(&file) else {
            continue;
        };
        for v in content
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        {
            let t = ts(&v);
            if t > until {
                continue;
            }
            if v["type"] == "start" {
                start = Some(v.clone());
            }
            if t >= since {
                rows.push(v);
            }
        }
    }
    (!rows.is_empty()).then_some(Run {
        label,
        live,
        start,
        rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn book_gaps_count_increments_across_restarts() {
        let statuses = [
            serde_json::json!({"book_gaps": [2, 1]}),
            serde_json::json!({"book_gaps": [3, 1]}),
            serde_json::json!({"book_gaps": [0, 2]}),
            serde_json::json!({"book_gaps": [1, 2]}),
        ];
        let refs: Vec<&Value> = statuses.iter().collect();
        assert_eq!(book_gaps(&refs), 3);
        assert_eq!(book_gaps(&[]), 0);
    }

    #[test]
    fn daily_report_once_at_eight_polish_time() {
        let dir = std::env::temp_dir().join(format!("arbibot-daily-{}", std::process::id()));
        let eight = 1_791_784_800_000.0;
        let marker = daily_due(&dir, eight).expect("8:00 czasu polskiego");
        fs::write(&marker, "").unwrap();
        assert!(daily_due(&dir, eight + 60_000.0).is_none());
        assert!(daily_due(&dir, eight - HOUR_MS).is_none());
        fs::remove_dir_all(&dir).unwrap();
    }
}
