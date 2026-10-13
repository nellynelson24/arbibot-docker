use std::time::Duration;

use account::{Bybit, Credentials, Fees, Okx, Scope};
use anyhow::{Context, Result};
use tokio::time::interval;
use venues::Venue;

use crate::book::{self, Book};

const MAX_AGE: Duration = Duration::from_secs(30);

#[derive(clap::Args)]
pub struct Args {
    #[arg(long, default_value = "KAIA/USDC")]
    #[arg(help = "Para, np. KAIA/USDC")]
    pair: String,
    #[arg(long, value_delimiter = ',', default_value = "10,15,20,25")]
    #[arg(help = "Kwoty zleceń w walucie kwotowania")]
    sizes: Vec<f64>,
    #[arg(long, default_value_t = 1)]
    #[arg(help = "Co ile sekund liczyć")]
    every_secs: u64,
    #[arg(long)]
    #[arg(help = "Wypisuj tylko chwile z zyskiem netto co najmniej tyle (bp); domyślnie wszystko")]
    min_bps: Option<f64>,
}

struct Direction {
    buy: Venue,
    sell: Venue,
    checks: Vec<u32>,
    profitable: Vec<u32>,
    best_bps: Vec<f64>,
}

pub async fn run(args: Args) -> Result<()> {
    let (base, quote) = args
        .pair
        .to_uppercase()
        .split_once('/')
        .map(|(b, q)| (b.to_owned(), q.to_owned()))
        .with_context(|| format!("{}: oczekiwano formatu BAZA/KWOTOWANIE", args.pair))?;

    let fees = account_fees(&base, &quote).await;
    println!(
        "{base}/{quote}: prowizje taker OKX EEA {:.3}%, Bybit EU {:.3}%; kwoty {:?} {quote}",
        fees.0.taker * 100.0,
        fees.1.taker * 100.0,
        args.sizes
    );
    let okx = book::spawn(Venue::Okx, &base, &quote);
    let bybit = book::spawn(Venue::Bybit, &base, &quote);

    let n = args.sizes.len();
    let mut directions =
        [(Venue::Okx, Venue::Bybit), (Venue::Bybit, Venue::Okx)].map(|(buy, sell)| Direction {
            buy,
            sell,
            checks: vec![0; n],
            profitable: vec![0; n],
            best_bps: vec![f64::MIN; n],
        });
    let taker = |venue: Venue| match venue {
        Venue::Okx => fees.0.taker,
        _ => fees.1.taker,
    };

    let mut every = interval(Duration::from_secs(args.every_secs.max(1)));
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            _ = every.tick() => {}
        }
        let books = [okx.borrow().clone(), bybit.borrow().clone()];
        let book_of = |venue: Venue| &books[usize::from(venue != Venue::Okx)];
        if books
            .iter()
            .any(|b| b.age().is_none_or(|age| age > MAX_AGE))
        {
            println!("  (czekam na aktualne książki obu giełd)");
            continue;
        }
        let mut line = Vec::new();
        let mut best_now = f64::MIN;
        for d in &mut directions {
            let nets: Vec<Option<f64>> = args
                .sizes
                .iter()
                .map(|&q| {
                    net_bps(
                        book_of(d.buy),
                        book_of(d.sell),
                        q,
                        taker(d.buy),
                        taker(d.sell),
                    )
                })
                .collect();
            for (i, net) in nets.iter().enumerate() {
                if let Some(net) = *net {
                    d.checks[i] += 1;
                    if net > 0.0 {
                        d.profitable[i] += 1;
                    }
                    d.best_bps[i] = d.best_bps[i].max(net);
                    best_now = best_now.max(net);
                }
            }
            let cells: Vec<String> = args
                .sizes
                .iter()
                .zip(&nets)
                .map(|(q, net)| {
                    net.map_or(format!("{q:.0}: brak głębokości"), |n| {
                        format!("{q:.0}: {n:+.1}")
                    })
                })
                .collect();
            line.push(format!(
                "{}→{} {}",
                d.buy.name(),
                d.sell.name(),
                cells.join(" ")
            ));
        }
        if args.min_bps.is_none_or(|min| best_now >= min) {
            println!("  {}", line.join("  |  "));
        }
    }

    println!("\nPodsumowanie (bp = 0,01%; zysk po prowizjach taker obu giełd):");
    for d in &directions {
        for (i, q) in args.sizes.iter().enumerate() {
            if d.checks[i] == 0 {
                continue;
            }
            println!(
                "  kup {:<8} sprzedaj {:<8} {q:>5.0} {quote}: z zyskiem {:>5.1}% pomiarów, najlepiej {:+.1} bp",
                d.buy.name(),
                d.sell.name(),
                d.profitable[i] as f64 * 100.0 / d.checks[i] as f64,
                d.best_bps[i]
            );
        }
    }
    Ok(())
}

fn net_bps(buy: &Book, sell: &Book, amount: f64, buy_fee: f64, sell_fee: f64) -> Option<f64> {
    let buy_price = buy.buy_price(amount)?;
    let sell_price = sell.sell_price(amount)?;
    Some((sell_price * (1.0 - sell_fee) / (buy_price * (1.0 + buy_fee)) - 1.0) * 1e4)
}

async fn account_fees(base: &str, quote: &str) -> (Fees, Fees) {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap_or_default();
    let fallback = |venue: Venue| Fees {
        maker: venue.maker_fee(),
        taker: venue.taker_fee(),
    };
    let okx = match Credentials::load(Venue::Okx, Scope::Read) {
        Ok(c) => Okx::new(http.clone(), c).fees(base, quote).await.ok(),
        Err(_) => None,
    };
    let bybit = match Credentials::load(Venue::Bybit, Scope::Read) {
        Ok(c) => Bybit::new(http, c).fees(base, quote).await.ok(),
        Err(_) => None,
    };
    if okx.is_none() || bybit.is_none() {
        eprintln!("Uwaga: część prowizji z tabeli domyślnej (brak odpowiedzi konta).");
    }
    (
        okx.unwrap_or_else(|| fallback(Venue::Okx)),
        bybit.unwrap_or_else(|| fallback(Venue::Bybit)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn net_uses_depth_and_both_fees() {
        let buy = Book {
            asks: vec![(1.0, 10.0), (1.1, 100.0)],
            ..Book::default()
        };
        let sell = Book {
            bids: vec![(1.2, 100.0)],
            ..Book::default()
        };

        let avg = 20.0 / (10.0 + 10.0 / 1.1);
        let expected = (1.2 * 0.998 / (avg * 1.0025) - 1.0) * 1e4;
        let net = net_bps(&buy, &sell, 20.0, 0.0025, 0.002).unwrap();
        assert!((net - expected).abs() < 1e-9);
        assert!(net_bps(&buy, &sell, 1_000.0, 0.0, 0.0).is_none());
    }
}
