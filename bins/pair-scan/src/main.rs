use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Result, anyhow};
use clap::Parser;
use futures_util::future::join_all;
use tokio::time::sleep;

use venues::{Book, Quote, Venue};

const MAX_OWN_SPREAD: f64 = 0.10;
const MAX_PLAUSIBLE_GAP: f64 = 0.10;

#[derive(Parser)]
#[command(about = "Szuka par z największą różnicą cen między giełdami (najlepsze oferty, migawki REST)", long_about = None)]
struct Args {
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u32).range(1..))]
    #[arg(help = "Liczba migawek")]
    rounds: u32,
    #[arg(long, default_value_t = 15)]
    #[arg(help = "Odstęp między migawkami (sekundy)")]
    interval_secs: u64,
    #[arg(long, value_delimiter = ',', default_value = "EUR,USDC")]
    #[arg(help = "Waluty kwotowania do porównania")]
    quotes: Vec<String>,
    #[arg(long, default_value_t = 10.0)]
    #[arg(help = "Minimalna wartość najlepszej oferty po obu stronach (w walucie kwotowania)")]
    min_depth: f64,
    #[arg(long, default_value_t = 20)]
    #[arg(help = "Ile tras pokazać")]
    top: usize,
    #[arg(long, default_value = "XRP")]
    #[arg(help = "Waluta bazowa pokazana osobno")]
    focus: String,
}

#[derive(PartialEq, Eq, Hash)]
struct Route {
    base: String,
    quote: String,
    buy: Venue,
    sell: Venue,
}

#[derive(Default)]
struct Samples {
    gross: Vec<f64>,
    net: Vec<f64>,
    depth: Vec<f64>,
}

struct Row<'a> {
    route: &'a Route,
    seen: usize,
    profitable: usize,
    gross: f64,
    net: f64,
    net_max: f64,
    depth: Option<f64>,
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow!("nie udało się ustawić dostawcy kryptografii rustls"))?;

    let args = Args::parse();
    let quotes: Vec<String> = args.quotes.iter().map(|q| q.to_uppercase()).collect();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;

    let mut routes: HashMap<Route, Samples> = HashMap::new();
    for round in 1..=args.rounds {
        if round > 1 {
            sleep(Duration::from_secs(args.interval_secs)).await;
        }
        let fetched = join_all(Venue::ALL.map(|venue| venue.fetch_quotes(&client))).await;
        let mut books = Vec::new();
        for (venue, book) in Venue::ALL.into_iter().zip(fetched) {
            match book {
                Ok(book) => books.push((venue, book)),
                Err(e) => eprintln!("  {}: pominięta w tej migawce ({e:#})", venue.name()),
            }
        }
        record(&books, &quotes, &mut routes);
        eprintln!("Migawka {round}/{}", args.rounds);
    }

    let rows = summarize(&routes, args.rounds as usize, args.min_depth);
    println!(
        "\nTrasy z najwyższą medianą zysku po prowizjach taker \
         ({} migawek co {} s, oferta ≥ {} w walucie kwotowania):\n",
        args.rounds, args.interval_secs, args.min_depth
    );
    print_rows(rows.iter().take(args.top));

    let focus = args.focus.to_uppercase();
    println!("\nNajlepsze trasy dla {focus}:\n");
    print_rows(rows.iter().filter(|r| r.route.base == focus).take(5));

    println!(
        "\nUwagi:\n\
         - Trasy z różnicą powyżej 10% pominięto: prawie zawsze to inny token pod tym samym\n  \
           symbolem (np. LUNA: Terra Classic vs Terra 2.0).\n\
         - Stała różnica kilku procent zwykle oznacza zablokowane wpłaty/wypłaty na jednej\n  \
           giełdzie albo pustą książkę. Sprawdź przed handlem.\n\
         - Bitstamp nie podaje wielkości ofert w zbiorczym tickerze (Oferta = ?).\n\
         - Migawki z różnych giełd dzieli do kilkuset ms: pojedynczy odczyt może być szumem,\n  \
           liczy się mediana i kolumna Zysk>0.\n\
         - Prowizje: najniższy próg taker, bez opłat za transfer."
    );
    Ok(())
}

fn record(books: &[(Venue, Book)], quotes: &[String], routes: &mut HashMap<Route, Samples>) {
    for (buy, buy_book) in books {
        for (sell, sell_book) in books {
            if buy == sell {
                continue;
            }
            for ((base, quote), at_buy) in buy_book {
                if !quotes.contains(quote) {
                    continue;
                }
                let Some(at_sell) = sell_book.get(&(base.clone(), quote.clone())) else {
                    continue;
                };
                if !is_live(at_buy) || !is_live(at_sell) {
                    continue;
                }
                let gross = at_sell.bid / at_buy.ask - 1.0;
                let net = at_sell.bid * (1.0 - sell.taker_fee())
                    / (at_buy.ask * (1.0 + buy.taker_fee()))
                    - 1.0;
                let samples = routes
                    .entry(Route {
                        base: base.clone(),
                        quote: quote.clone(),
                        buy: *buy,
                        sell: *sell,
                    })
                    .or_default();
                samples.gross.push(gross);
                samples.net.push(net);
                if let (Some(ask_qty), Some(bid_qty)) = (at_buy.ask_qty, at_sell.bid_qty) {
                    samples
                        .depth
                        .push((ask_qty * at_buy.ask).min(bid_qty * at_sell.bid));
                }
            }
        }
    }
}

fn is_live(q: &Quote) -> bool {
    q.bid > 0.0 && q.ask >= q.bid && (q.ask - q.bid) / q.ask <= MAX_OWN_SPREAD
}

fn summarize(routes: &HashMap<Route, Samples>, rounds: usize, min_depth: f64) -> Vec<Row<'_>> {
    let mut rows: Vec<Row> = routes
        .iter()
        .filter(|(_, s)| s.net.len() * 2 >= rounds)
        .map(|(route, s)| Row {
            route,
            seen: s.net.len(),
            profitable: s.net.iter().filter(|&&n| n > 0.0).count(),
            gross: median(&s.gross),
            net: median(&s.net),
            net_max: s.net.iter().copied().fold(f64::MIN, f64::max),
            depth: (!s.depth.is_empty()).then(|| median(&s.depth)),
        })
        .filter(|r| r.depth.is_none_or(|d| d >= min_depth))
        .filter(|r| r.gross.abs() <= MAX_PLAUSIBLE_GAP)
        .collect();
    rows.sort_by(|a, b| b.net.total_cmp(&a.net));
    rows
}

fn print_rows<'a>(rows: impl Iterator<Item = &'a Row<'a>>) {
    println!(
        "{:<14} {:<9} {:<9} {:>7} {:>11} {:>11} {:>10} {:>10}",
        "Para", "Kup", "Sprzedaj", "Zysk>0", "Różnica", "Po prow.", "Po prow.", "Oferta"
    );
    println!(
        "{:<14} {:<9} {:<9} {:>7} {:>11} {:>11} {:>10} {:>10}",
        "", "", "", "", "p50", "p50", "max", "p50"
    );
    for r in rows {
        let depth = r.depth.map_or("?".to_owned(), |d| format!("{d:.0}"));
        println!(
            "{:<14} {:<9} {:<9} {:>7} {:>10.2}% {:>10.2}% {:>9.2}% {:>10}",
            format!("{}/{}", r.route.base, r.route.quote),
            r.route.buy.name(),
            r.route.sell.name(),
            format!("{}/{}", r.profitable, r.seen),
            r.gross * 100.0,
            r.net * 100.0,
            r.net_max * 100.0,
            depth
        );
    }
}

fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[sorted.len() / 2]
}
