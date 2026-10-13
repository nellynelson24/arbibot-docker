use std::time::Duration;

use account::{Balance, Bybit, Credentials, Network, Okx, Scope};
use anyhow::{Context, Result};
use venues::Venue;

#[derive(clap::Args)]
pub struct Args {
    #[arg(long, value_delimiter = ',', default_value = "KAIA/USDC,IMX/USDC")]
    #[arg(help = "Pary do sprawdzenia, np. KAIA/USDC,IMX/USDC")]
    pairs: Vec<String>,
}

pub async fn run(args: Args) -> Result<()> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let okx = Okx::new(http.clone(), Credentials::load(Venue::Okx, Scope::Read)?);
    let bybit = Bybit::new(http, Credentials::load(Venue::Bybit, Scope::Read)?);

    println!("== Salda (niezerowe)");
    print_balances("OKX EEA", okx.balances().await);
    print_balances("Bybit EU", bybit.balances().await);

    let mut assets: Vec<String> = Vec::new();
    for pair in &args.pairs {
        let (base, quote) = pair
            .to_uppercase()
            .split_once('/')
            .map(|(b, q)| (b.to_owned(), q.to_owned()))
            .with_context(|| format!("{pair}: oczekiwano formatu BAZA/KWOTOWANIE"))?;
        println!("\n== {base}/{quote}");
        match okx.fees(&base, &quote).await {
            Ok(f) => println!(
                "  OKX EEA  prowizje: maker {:.3}%  taker {:.3}%",
                f.maker * 100.0,
                f.taker * 100.0
            ),
            Err(e) => println!("  OKX EEA  prowizje: BŁĄD {e:#}"),
        }
        match bybit.fees(&base, &quote).await {
            Ok(f) => println!(
                "  Bybit EU prowizje: maker {:.3}%  taker {:.3}%",
                f.maker * 100.0,
                f.taker * 100.0
            ),
            Err(e) => println!("  Bybit EU prowizje: BŁĄD {e:#}"),
        }
        match okx.rules(&base, &quote).await {
            Ok(r) => println!(
                "  OKX EEA  reguły: krok ceny {} krok ilości {} min. ilość {}",
                r.tick, r.step, r.min_qty
            ),
            Err(e) => println!("  OKX EEA  reguły: BŁĄD {e:#}"),
        }
        match bybit.rules(&base, &quote).await {
            Ok(r) => println!(
                "  Bybit EU reguły: krok ceny {} krok ilości {} min. ilość {} min. wartość {}",
                r.tick,
                r.step,
                r.min_qty,
                r.min_notional.map_or("—".to_owned(), |v| v.to_string())
            ),
            Err(e) => println!("  Bybit EU reguły: BŁĄD {e:#}"),
        }
        for asset in [base, quote] {
            if !assets.contains(&asset) {
                assets.push(asset);
            }
        }
    }

    println!("\n== Sieci wypłat wspólne dla obu giełd (opłata w jednostkach aktywa)");
    for asset in &assets {
        let (okx_nets, bybit_nets) = match (okx.networks(asset).await, bybit.networks(asset).await)
        {
            (Ok(o), Ok(b)) => (o, b),
            (o, b) => {
                println!(
                    "  {asset}: BŁĄD OKX {:?} / Bybit {:?}",
                    o.err().map(|e| e.to_string()),
                    b.err().map(|e| e.to_string())
                );
                continue;
            }
        };
        let common: Vec<(&Network, &Network)> = okx_nets
            .iter()
            .filter_map(|o| {
                bybit_nets
                    .iter()
                    .find(|b| b.key() == o.key())
                    .map(|b| (o, b))
            })
            .collect();
        if common.is_empty() {
            println!(
                "  {asset}: brak wspólnej sieci (OKX: {}; Bybit: {})",
                names(&okx_nets),
                names(&bybit_nets)
            );
            continue;
        }
        for (o, b) in common {
            println!(
                "  {asset:<6} {:<12} OKX→Bybit: wypłata {} min {} {}| Bybit→OKX: wypłata {} min {} {}",
                o.key(),
                opt(o.withdraw_fee),
                opt(o.withdraw_min),
                status(o.can_withdraw && b.can_deposit),
                opt(b.withdraw_fee),
                opt(b.withdraw_min),
                status(b.can_withdraw && o.can_deposit),
            );
        }
    }
    Ok(())
}

fn print_balances(venue: &str, balances: Result<Vec<Balance>>) {
    match balances {
        Ok(list) => {
            let nonzero: Vec<&Balance> = list.iter().filter(|b| b.total > 0.0).collect();
            if nonzero.is_empty() {
                println!("  {venue}: połączono, brak środków");
            }
            for b in nonzero {
                println!(
                    "  {venue:<9} {:<8} {:<7} razem {:.6} dostępne {:.6}",
                    b.account, b.asset, b.total, b.available
                );
            }
        }
        Err(e) => println!("  {venue}: BŁĄD {e:#}"),
    }
}

fn names(networks: &[Network]) -> String {
    networks
        .iter()
        .map(|n| n.chain.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn opt(v: Option<f64>) -> String {
    v.map_or("?".to_owned(), |x| format!("{x}"))
}

fn status(open: bool) -> &'static str {
    if open { "" } else { "(zamknięta) " }
}
