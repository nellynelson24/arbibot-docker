mod forecast;
mod maker;
mod transit;

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Args;
use serde_json::Value;
use venues::Venue;

use crate::clock::{now_ms, utc_date};
use crate::transit::TRANSFER_SECS;
use forecast::{print_forecast, repeatable};
use maker::print_maker;
use transit::{
    TransitRecord, nearest_transfer, print_transit, read_transit, status_secs, transit_episodes,
};

const CAPITALS: [f64; 3] = [100.0, 1_000.0, 10_000.0];
const BUCKETS: f64 = 4.0;
const MIN_FORECAST_SECS: f64 = 3_600.0;
const DURATIONS: [(f64, &str); 7] = [
    (50.0, "<50 ms"),
    (100.0, "50–100 ms"),
    (250.0, "100–250 ms"),
    (1_000.0, "0,25–1 s"),
    (5_000.0, "1–5 s"),
    (60_000.0, "5–60 s"),
    (f64::INFINITY, ">60 s"),
];
const TOP_ROUTES: usize = 15;
const TOP_FORECASTS: usize = 5;

#[derive(Args)]
pub struct ReportArgs {
    #[arg(long)]
    #[arg(help = "Dzień (RRRR-MM-DD, UTC); domyślnie dziś")]
    date: Option<String>,
    #[arg(long, default_value = "data")]
    #[arg(help = "Katalog z danymi skanera")]
    data_dir: PathBuf,
    #[arg(long, default_value_t = 100.0)]
    #[arg(help = "Czas reakcji bota: krótsze okazje są nie do złapania (ms)")]
    reaction_ms: f64,
    #[arg(long, default_value_t = 60.0)]
    #[arg(help = "Dłuższe okazje to stała różnica (bariera transferu), poza prognozą (sekundy)")]
    persistent_secs: f64,
    #[arg(long, default_value_t = 0.0)]
    #[arg(help = "Tylko okazje z zyskiem po prowizjach co najmniej tyle (bp; 400 = 4%)")]
    min_net_bps: f64,
    #[arg(long, default_value_t = 60.0)]
    #[arg(
        help = "Modele cykliczne: czas przelewu między giełdami, od wypłaty do zaksięgowania (s)"
    )]
    transfer_secs: f64,
    #[arg(long, default_value_t = 0.3)]
    #[arg(help = "Modele cykliczne: opłata za wypłatę na cykl (waluta kwotowania)")]
    withdraw_fee: f64,
    #[arg(long, value_delimiter = ',', default_value = "LUNA,BTT,WIN")]
    #[arg(help = "Modele cykliczne: aktywa bez możliwego transferu (inna sieć, podatek sieci)")]
    exclude: Vec<String>,
}

struct Transfer {
    ms: f64,
    fee: f64,
    excluded: Vec<String>,
}

impl Transfer {
    fn allows(&self, e: &Episode) -> bool {
        e.duration_ms >= self.ms
            && !self
                .excluded
                .iter()
                .any(|asset| e.pair.split('/').next() == Some(asset.as_str()))
    }
}

type Key<'a> = (&'a str, &'a str, &'a str);

struct Episode {
    pair: String,
    buy: String,
    sell: String,
    start_ms: f64,
    duration_ms: f64,
    first_net_bps: f64,
    first_size: Option<f64>,
    best_net_bps: f64,
}

impl Episode {
    fn parse(v: &Value) -> Option<Self> {
        Some(Episode {
            pair: v["pair"].as_str()?.to_owned(),
            buy: venue_name(v["buy"].as_str()?),
            sell: venue_name(v["sell"].as_str()?),
            start_ms: v["start_ms"].as_f64()?,
            duration_ms: v["duration_ms"].as_f64()?,
            first_net_bps: v["first_net_bps"].as_f64()?,
            first_size: v["first_size"].as_f64(),
            best_net_bps: v["best_net_bps"].as_f64()?,
        })
    }

    fn route(&self) -> Key<'_> {
        (&self.pair, &self.buy, &self.sell)
    }

    fn market(&self) -> Key<'_> {
        let (a, b) = if self.buy < self.sell {
            (&self.buy, &self.sell)
        } else {
            (&self.sell, &self.buy)
        };
        (&self.pair, a, b)
    }
}

pub fn run(args: &ReportArgs) -> Result<()> {
    let date = args.date.clone().unwrap_or_else(|| utc_date(now_ms()));
    let path = args
        .data_dir
        .join("opportunities")
        .join(format!("{date}.jsonl"));
    let text =
        fs::read_to_string(&path).with_context(|| format!("brak danych: {}", path.display()))?;

    let mut covered_secs = 0.0;
    let mut episodes = Vec::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match v["type"].as_str() {
            Some("status") if v["live"].as_array().is_some_and(|l| l.len() >= 2) => {
                covered_secs += v["interval_secs"].as_f64().unwrap_or(0.0);
            }
            Some("episode") => episodes.extend(Episode::parse(&v)),
            _ => {}
        }
    }
    if covered_secs == 0.0 {
        bail!(
            "{}: brak zapisów stanu — uruchom `scanner run` na dłużej niż minutę",
            path.display()
        );
    }
    episodes.sort_by(|a, b| a.start_ms.total_cmp(&b.start_ms));

    println!(
        "Raport {date}: skaner działał {:.1} h (co najmniej 2 giełdy na żywo)\n",
        covered_secs / 3600.0
    );
    if args.min_net_bps > 0.0 {
        episodes.retain(|e| e.best_net_bps >= args.min_net_bps);
        for e in &mut episodes {
            e.first_net_bps = e.first_net_bps.max(args.min_net_bps);
        }
        println!(
            "Próg: tylko okazje z zyskiem ≥ {:.2}% (czas trwania liczony od zysku > 0, \
             więc dla wysokiego progu optymistyczny)",
            args.min_net_bps / 100.0
        );
    }
    println!("Okazje z zyskiem po prowizjach taker: {}", episodes.len());
    print_durations(&episodes);

    let persistent_ms = args.persistent_secs * 1000.0;
    let catchable: Vec<&Episode> = episodes
        .iter()
        .filter(|e| e.duration_ms >= args.reaction_ms && e.duration_ms < persistent_ms)
        .collect();
    let persistent: Vec<&Episode> = episodes
        .iter()
        .filter(|e| e.duration_ms >= persistent_ms)
        .collect();
    println!(
        "Do złapania przy reakcji {} ms (i krótsze niż {} s): {}\n",
        args.reaction_ms,
        args.persistent_secs,
        catchable.len()
    );

    print_routes(&catchable);
    print_persistent(&persistent, args.persistent_secs);
    let excluded: Vec<String> = args.exclude.iter().map(|a| a.to_uppercase()).collect();
    let (transit, transit_secs) = read_transit(&args.data_dir, &date);
    let transit: Vec<TransitRecord> = transit
        .into_iter()
        .filter(|r| {
            !excluded
                .iter()
                .any(|asset| r.pair.split('/').next() == Some(asset.as_str()))
        })
        .collect();
    print_transit(&transit, args.withdraw_fee);
    if covered_secs < MIN_FORECAST_SECS {
        println!(
            "Prognoza pominięta: {:.0} min danych to za mało (potrzeba co najmniej {:.0} min).",
            covered_secs / 60.0,
            MIN_FORECAST_SECS / 60.0
        );
    } else {
        let transfer = Transfer {
            ms: args.transfer_secs * 1000.0,
            fee: args.withdraw_fee,
            excluded,
        };

        let measured = transit_episodes(&transit, args.transfer_secs);
        let (all, cyclic_secs): (Vec<&Episode>, f64) = if measured.is_empty() {
            (episodes.iter().collect(), covered_secs)
        } else {
            let secs = if transit_secs > 0.0 {
                transit_secs
            } else {
                covered_secs
            };
            println!(
                "Modele 2–3: cena sprzedaży zmierzona {} s po kupnie (dane transit, {:.1} h).",
                nearest_transfer(args.transfer_secs),
                secs / 3600.0
            );
            (measured.iter().collect(), secs)
        };
        print_forecast(
            &catchable,
            &all,
            &transfer,
            86_400.0 / covered_secs,
            86_400.0 / cyclic_secs,
        );
    }
    print_maker(&args.data_dir, &date)?;
    Ok(())
}

fn print_durations(episodes: &[Episode]) {
    let mut counts = [0usize; DURATIONS.len()];
    for e in episodes {
        let i = DURATIONS
            .iter()
            .position(|(limit, _)| e.duration_ms < *limit)
            .unwrap_or(DURATIONS.len() - 1);
        counts[i] += 1;
    }
    let parts: Vec<String> = DURATIONS
        .iter()
        .zip(counts)
        .map(|((_, label), n)| format!("{label}: {n}"))
        .collect();
    println!("  czas trwania — {}", parts.join(" | "));
}

fn print_routes(catchable: &[&Episode]) {
    let mut by_route: HashMap<Key, Vec<&Episode>> = HashMap::new();
    for e in catchable {
        by_route.entry(e.route()).or_default().push(e);
    }
    let mut routes: Vec<_> = by_route.into_iter().collect();
    routes.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));

    println!("Najczęstsze trasy do złapania:");
    println!(
        "{:<14} {:<9} {:<9} {:>8} {:>10} {:>10} {:>10}",
        "Para", "Kup", "Sprzedaj", "Liczba", "Czas p50", "Zysk p50", "Oferta p50"
    );
    for ((pair, buy, sell), list) in routes.iter().take(TOP_ROUTES) {
        let sizes: Vec<f64> = list.iter().filter_map(|e| e.first_size).collect();
        let size = if sizes.is_empty() {
            "?".to_owned()
        } else {
            format!("{:.0}", median(&sizes))
        };
        println!(
            "{:<14} {:<9} {:<9} {:>8} {:>7.0} ms {:>7.1} bp {:>10}",
            pair,
            buy,
            sell,
            list.len(),
            median(&list.iter().map(|e| e.duration_ms).collect::<Vec<_>>()),
            median(&list.iter().map(|e| e.first_net_bps).collect::<Vec<_>>()),
            size
        );
    }
    println!();
}

fn print_persistent(persistent: &[&Episode], persistent_secs: f64) {
    if persistent.is_empty() {
        return;
    }
    let mut by_route: HashMap<Key, (f64, f64)> = HashMap::new();
    for e in persistent {
        let (total_ms, best) = by_route.entry(e.route()).or_insert((0.0, f64::MIN));
        *total_ms += e.duration_ms;
        *best = best.max(e.best_net_bps);
    }
    let mut routes: Vec<_> = by_route.into_iter().collect();
    routes.sort_by(|a, b| b.1.0.total_cmp(&a.1.0).then_with(|| a.0.cmp(&b.0)));

    println!("Stałe różnice (≥ {persistent_secs} s), poza prognozą — zwykle bariera transferu:");
    println!(
        "{:<14} {:<9} {:<9} {:>11} {:>10}",
        "Para", "Kup", "Sprzedaj", "Łącznie", "Zysk max"
    );
    for ((pair, buy, sell), (total_ms, best)) in routes.iter().take(TOP_ROUTES) {
        println!(
            "{:<14} {:<9} {:<9} {:>7.0} min {:>7.1} bp",
            pair,
            buy,
            sell,
            total_ms / 60_000.0,
            best
        );
    }
    println!();
}

fn venue_name(id: &str) -> String {
    Venue::ALL
        .into_iter()
        .find(|v| v.id() == id)
        .map_or_else(|| id.to_owned(), |v| v.name().to_owned())
}

fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[sorted.len() / 2]
}

#[cfg(test)]
mod tests {
    use super::forecast::{simulate, simulate_cyclic};
    use super::*;

    const NO_TRANSFER: Transfer = Transfer {
        ms: 0.0,
        fee: 0.0,
        excluded: Vec::new(),
    };

    #[test]
    fn only_two_way_volume_repeats() {
        assert_eq!(
            repeatable(10.0, 100.0, 0.0),
            0.0,
            "one-sided: inventory runs out"
        );
        assert_eq!(repeatable(10.0, 50.0, 50.0), 10.0, "balanced: all repeats");
        assert!((repeatable(9.0, 100.0, 50.0) - 6.0).abs() < 1e-12);
    }

    #[test]
    fn transit_uses_measured_sell_and_skips_missed_buys() {
        let record = |alive: bool| TransitRecord {
            pair: "XRP/EUR".to_owned(),
            buy: "A".to_owned(),
            sell: "B".to_owned(),
            start_ms: 0.0,
            entry_size: Some(500.0),
            alive,
            sells: TRANSFER_SECS
                .iter()
                .map(|&s| (Some(-(s as f64)), Some(200.0)))
                .collect(),
        };
        let episodes = transit_episodes(&[record(true), record(false)], 60.0);
        assert_eq!(episodes.len(), 1, "gap closed before the bot could buy");
        assert_eq!(episodes[0].first_net_bps, -60.0);
        assert_eq!(episodes[0].first_size, Some(200.0));
        assert_eq!(nearest_transfer(50.0), 60);
    }

    #[test]
    fn cyclic_waits_for_transfer_and_pays_fee() {
        let mut a_to_b = episode("A", "B", 50.0, 1_000.0);
        a_to_b.duration_ms = 120_000.0;
        let mut b_to_a = episode("B", "A", 50.0, 1_000.0);
        b_to_a.duration_ms = 120_000.0;
        b_to_a.start_ms = 30_000.0;
        let transfer = Transfer {
            ms: 60_000.0,
            fee: 0.2,
            excluded: Vec::new(),
        };
        let (profit, cycles) = simulate_cyclic(&[&a_to_b, &b_to_a], "A", 100.0, &transfer);
        assert_eq!(cycles, 1);
        assert!((profit - (100.0 * 50.0 / 1e4 - 0.2)).abs() < 1e-9);
    }

    #[test]
    fn transfer_rejects_short_gaps_and_excluded_assets() {
        let transfer = Transfer {
            ms: 60_000.0,
            fee: 0.0,
            excluded: vec!["XRP".to_owned()],
        };
        let mut long = episode("A", "B", 50.0, 1_000.0);
        long.duration_ms = 61_000.0;
        assert!(!transfer.allows(&long), "XRP excluded");
        long.pair = "ADA/EUR".to_owned();
        assert!(transfer.allows(&long));
        long.duration_ms = 59_000.0;
        assert!(!transfer.allows(&long), "gap closes before the deposit");
    }

    fn episode(buy: &str, sell: &str, net_bps: f64, size: f64) -> Episode {
        Episode {
            pair: "XRP/EUR".to_owned(),
            buy: buy.to_owned(),
            sell: sell.to_owned(),
            start_ms: 0.0,
            duration_ms: 500.0,
            first_net_bps: net_bps,
            first_size: Some(size),
            best_net_bps: net_bps,
        }
    }

    #[test]
    fn trades_stop_when_one_direction_drains_inventory() {
        let a_to_b = episode("A", "B", 10.0, 1_000.0);

        let (profit, _) = simulate(&[&a_to_b, &a_to_b], "A", 100.0);
        assert!((profit - 25.0 * 10.0 / 1e4).abs() < 1e-12);
    }

    #[test]
    fn reverse_trade_refills_inventory() {
        let a_to_b = episode("A", "B", 10.0, 1_000.0);
        let b_to_a = episode("B", "A", 20.0, 1_000.0);

        let (profit, _) = simulate(&[&a_to_b, &b_to_a], "A", 100.0);
        assert!((profit - (25.0 * 10.0 + 50.0 * 20.0) / 1e4).abs() < 1e-12);
    }

    #[test]
    fn trade_limited_by_offer_size() {
        let small = episode("A", "B", 10.0, 5.0);
        let (profit, _) = simulate(&[&small], "A", 100.0);
        assert!((profit - 5.0 * 10.0 / 1e4).abs() < 1e-12);
    }

    #[test]
    fn cyclic_pair_uses_full_capital() {
        let a_to_b = episode("A", "B", 10.0, 1_000.0);
        let b_to_a = episode("B", "A", 20.0, 1_000.0);

        let (profit, _) = simulate_cyclic(&[&a_to_b, &b_to_a], "A", 100.0, &NO_TRANSFER);
        let expected = (100.0 * 10.0 / 1e4) + (100.10 * 20.0 / 1e4);
        assert!((profit - expected).abs() < 1e-6);
    }

    #[test]
    fn cyclic_pair_waits_for_return_direction() {
        let a_to_b = episode("A", "B", 10.0, 1_000.0);

        let (profit, _) = simulate_cyclic(&[&a_to_b, &a_to_b], "A", 100.0, &NO_TRANSFER);
        assert!((profit - 100.0 * 10.0 / 1e4).abs() < 1e-6);
    }

    #[test]
    fn cyclic_global_multi_hop() {
        let a_to_b = episode("A", "B", 10.0, 1_000.0);
        let b_to_c = episode("B", "C", 15.0, 1_000.0);
        let (profit, count) = simulate_cyclic(&[&a_to_b, &b_to_c], "A", 100.0, &NO_TRANSFER);
        assert_eq!(count, 2);
        let expected = (100.0 * 10.0 / 1e4) + (100.10 * 15.0 / 1e4);
        assert!((profit - expected).abs() < 1e-6);
    }
}
