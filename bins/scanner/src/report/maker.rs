use std::collections::HashMap;
use std::fs;

use anyhow::Result;
use serde_json::Value;

use super::*;

#[derive(Default)]
pub(super) struct MakerTotals {
    fills: u32,
    bought: f64,
    sold: f64,
    profit: f64,
    capital: f64,
    first_price: f64,
    last_price: f64,
}

impl MakerTotals {
    fn repeatable(&self) -> f64 {
        repeatable(self.profit, self.bought, self.sold)
    }

    fn inventory_change(&self) -> f64 {
        if self.first_price > 0.0 {
            self.capital / 2.0 * (self.last_price / self.first_price - 1.0)
        } else {
            0.0
        }
    }
}

pub(super) fn print_maker(data_dir: &std::path::Path, date: &str) -> Result<()> {
    let path = data_dir.join("maker").join(format!("{date}.jsonl"));
    let Ok(text) = fs::read_to_string(&path) else {
        return Ok(());
    };
    let lines: Vec<Value> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect();
    let covered_secs = status_secs(&lines);
    if covered_secs == 0.0 {
        return Ok(());
    }
    let mut by_market: HashMap<(String, String, String), MakerTotals> = HashMap::new();
    let unhedged = lines
        .iter()
        .filter(|v| v["type"] == "fill" && v["unhedged"] == true)
        .count();
    for v in lines.iter().filter(|v| v["type"] == "fill") {
        let (Some(pair), Some(maker), Some(hedge), Some(size), Some(profit)) = (
            v["pair"].as_str(),
            v["maker"].as_str(),
            v["hedge"].as_str(),
            v["size"].as_f64(),
            v["profit"].as_f64(),
        ) else {
            continue;
        };
        let entry = by_market
            .entry((pair.to_owned(), venue_name(maker), venue_name(hedge)))
            .or_default();
        entry.fills += 1;
        if v["side"] == "buy" {
            entry.bought += size;
        } else {
            entry.sold += size;
        }
        entry.profit += profit;
        entry.capital = v["capital"].as_f64().unwrap_or(0.0);
        if let Some(price) = v["price"].as_f64() {
            if entry.first_price == 0.0 {
                entry.first_price = price;
            }
            entry.last_price = price;
        }
    }
    let funding = read_funding(data_dir, date);
    let hours = covered_secs / 3600.0;
    let mut rows: Vec<MakerRow> = by_market
        .into_iter()
        .map(|((pair, maker, hedge), totals)| {
            let asset = pair.split('/').next().unwrap_or_default();

            let funding = funding
                .get(asset)
                .map(|apr| apr * totals.capital / 2.0 * hours / HOURS_PER_YEAR);
            let result = match funding {
                Some(f) => totals.repeatable() + f,
                None => totals.repeatable() + totals.inventory_change(),
            };
            MakerRow {
                pair,
                maker,
                hedge,
                totals,
                funding,
                result,
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        b.result
            .total_cmp(&a.result)
            .then_with(|| (&a.pair, &a.maker, &a.hedge).cmp(&(&b.pair, &b.maker, &b.hedge)))
    });
    let total: f64 = rows.iter().map(|r| r.totals.profit).sum();
    let total_repeatable: f64 = rows.iter().map(|r| r.totals.repeatable()).sum();
    let capital = rows.first().map_or(0.0, |r| r.totals.capital);

    println!(
        "\nMarket making z zabezpieczeniem (symulacja; zlecenie maker na jednej giełdzie,\n\
         natychmiastowe zabezpieczenie taker na drugiej; kapitał {capital:.0} na każdy rynek):"
    );
    println!(
        "Symulacja działała {hours:.1} h. Wykonań bez możliwego zabezpieczenia (brak ceny \
         na drugiej giełdzie — otwarta pozycja): {unhedged}."
    );
    println!(
        "Wykonań: {}, rynków: {}, zysk łącznie (każdy rynek z osobnym kapitałem): {total:.2},\n\
         z tego powtarzalny (obrót w obie strony): {total_repeatable:.2}",
        rows.iter().map(|r| r.totals.fills).sum::<u32>(),
        rows.len()
    );
    let forecast = covered_secs >= MIN_FORECAST_SECS;
    let month = 86_400.0 / covered_secs * 30.0;
    println!(
        "{:<12} {:<9} {:<9} {:>7} {:>13} {:>9} {:>11} {:>9} {:>9} {:>9} {:>11}",
        "Para",
        "Maker",
        "Zabezp.",
        "Wykonań",
        "Kup./Sprzed.",
        "Marża bp",
        "Powtarzalny",
        "Zapas ±",
        "Finans.",
        "Wynik",
        "Mies.(wyn.)"
    );
    for r in rows.iter().take(TOP_ROUTES) {
        let t = &r.totals;
        let monthly = if forecast {
            format!("{:.2}", r.result * month)
        } else {
            "—".to_owned()
        };
        println!(
            "{:<12} {:<9} {:<9} {:>7} {:>13} {:>9.1} {:>11.2} {:>+9.2} {:>9} {:>+9.2} {monthly:>11}",
            r.pair,
            r.maker,
            r.hedge,
            t.fills,
            format!("{:.0}/{:.0}", t.bought, t.sold),
            t.profit / (t.bought + t.sold) * 1e4,
            t.repeatable(),
            t.inventory_change(),
            r.funding.map_or("brak".to_owned(), |f| format!("{f:+.2}")),
            r.result
        );
    }
    if forecast {
        let hedged: Vec<f64> = rows
            .iter()
            .filter(|r| r.funding.is_some())
            .map(|r| r.result * month)
            .collect();
        println!(
            "Prognoza miesięczna, tylko rynki z kontraktem (bez ryzyka ceny): najlepszy {:.2} \
             (kapitał {capital:.0}); 3 najlepsze {:.2} (kapitał {:.0}).",
            hedged.first().copied().unwrap_or(0.0),
            hedged.iter().take(3).sum::<f64>(),
            capital * 3.0
        );
    }
    println!(
        "Założenia: wykonanie tylko przez prawdziwe transakcje i po kolejce przed nami;\n\
         zabezpieczenie po cenie drugiej giełdy po opóźnieniu. Wynik: z kontraktem perpetual na\n\
         Krakenie = powtarzalny + finansowanie krótkiej pozycji na 50% kapitału; bez kontraktu\n\
         (\"brak\") = powtarzalny + zmiana wartości trzymanego aktywa (Zapas)."
    );
    Ok(())
}

pub(super) struct MakerRow {
    pair: String,
    maker: String,
    hedge: String,
    totals: MakerTotals,
    funding: Option<f64>,
    result: f64,
}

pub(super) const HOURS_PER_YEAR: f64 = 24.0 * 365.0;

pub(super) fn read_funding(data_dir: &std::path::Path, date: &str) -> HashMap<String, f64> {
    let path = data_dir.join("funding").join(format!("{date}.jsonl"));
    let Ok(text) = fs::read_to_string(path) else {
        return HashMap::new();
    };
    let mut sums: HashMap<String, (f64, u32)> = HashMap::new();
    for v in text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
    {
        for (asset, apr) in v["apr"].as_object().into_iter().flatten() {
            if let Some(apr) = apr.as_f64() {
                let entry = sums.entry(asset.clone()).or_default();
                entry.0 += apr;
                entry.1 += 1;
            }
        }
    }
    sums.into_iter()
        .map(|(asset, (sum, n))| (asset, sum / f64::from(n)))
        .collect()
}
