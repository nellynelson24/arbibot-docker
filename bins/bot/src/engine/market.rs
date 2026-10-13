use std::collections::HashMap;
use std::path::Path;

use account::{Bybit, Network, Okx};
use anyhow::{Context, Result};
use venues::Venue;

use super::{Accounts, Args, MAX_ROUTE_COST, Market};
use crate::ledger::now_ms;
use crate::selector;

const CANDIDATES: usize = 10;
pub(super) const REVIEW_DAYS: u32 = 3;

pub(super) type Routes = [Option<f64>; 2];

pub(super) struct Pick {
    pub(super) pair: String,
    pub(super) base: String,
    pub(super) quote: String,
    pub(super) base_fee: Routes,
    pub(super) quote_fee: Routes,
    pub(super) profit: f64,
}

pub(super) async fn load_market(
    args: &Args,
    http: &reqwest::Client,
    accounts: &Accounts,
) -> Result<Market> {
    let (base, quote, base_fee, quote_fee) = match &args.pair {
        Some(pair) => {
            let (base, quote) = split_pair(pair)?;
            let base_fee = routes(accounts, &base).await?;
            let quote_fee = routes(accounts, &quote).await?;
            (base, quote, base_fee, quote_fee)
        }
        None => {
            let route_limit = MAX_ROUTE_COST * args.capital;
            let (_, pick) = picks(
                &args.data_dir,
                args.size,
                args.min_bps,
                route_limit,
                http,
                accounts,
            )
            .await?;
            let pick = pick.with_context(|| {
                format!(
                    "brak pary OKX↔Bybit z okazjami ≥ {} bp i tanim rebalansem w danych skanera \
                     z ostatnich {REVIEW_DAYS} dni — podaj parę: --pair STRK/USDC",
                    args.min_bps
                )
            })?;
            (pick.base, pick.quote, pick.base_fee, pick.quote_fee)
        }
    };
    load_pair(accounts, base, quote, base_fee, quote_fee).await
}

pub(super) async fn load_pair(
    accounts: &Accounts,
    base: String,
    quote: String,
    base_fee: Routes,
    quote_fee: Routes,
) -> Result<Market> {
    let (okx, bybit) = (&accounts.okx, &accounts.bybit);
    let fees = [
        okx.fees(&base, &quote).await.context("prowizje OKX")?,
        bybit.fees(&base, &quote).await.context("prowizje Bybit")?,
    ];
    let rules = [
        okx.rules(&base, &quote).await.context("reguły OKX")?,
        bybit.rules(&base, &quote).await.context("reguły Bybit")?,
    ];
    let step = rules[0].step.max(rules[1].step);
    let min_qty = rules[0].min_qty.max(rules[1].min_qty);
    let min_notional = rules
        .iter()
        .filter_map(|r| r.min_notional)
        .fold(0.0, f64::max);

    println!(
        "{base}/{quote}: taker OKX {:.2}% / Bybit {:.2}%, krok ilości {step}, min. {min_qty} {base} \
         i {min_notional} {quote}",
        fees[0].taker * 100.0,
        fees[1].taker * 100.0
    );
    for (asset, fee) in [(&base, base_fee), (&quote, quote_fee)] {
        match fee {
            [Some(a), Some(b)] => println!("  przelew {asset}: OKX→Bybit {a}, Bybit→OKX {b}"),
            _ => println!(
                "  przelew {asset}: brak sieci otwartej w obie strony — rebalans niemożliwy (szczegóły: bot check)"
            ),
        }
    }
    Ok(Market {
        base,
        quote,
        fees,
        tick: [rules[0].tick, rules[1].tick],
        step,
        min_qty,
        min_notional,
        base_fee,
        quote_fee,
    })
}

pub(super) async fn picks(
    data_dir: &Path,
    size: f64,
    min_bps: f64,
    route_limit: f64,
    http: &reqwest::Client,
    accounts: &Accounts,
) -> Result<(Vec<selector::Candidate>, Option<Pick>)> {
    let fee_bps = (Venue::Okx.taker_fee() + Venue::Bybit.taker_fee()) * 1e4;
    let ranked = selector::rank(data_dir, now_ms(), REVIEW_DAYS, fee_bps, min_bps, size);
    if ranked.is_empty() {
        return Ok((ranked, None));
    }

    let prices = Venue::Okx.fetch_quotes(http).await.context("ceny OKX")?;
    println!(
        "Selektor (ostatnie {REVIEW_DAYS} dni, okazje ≥ {min_bps} bp po prowizjach {fee_bps:.0} bp, \
         transakcja {size}; limit rebalansu {route_limit:.2}):"
    );
    let mut quote_routes: HashMap<String, Routes> = HashMap::new();
    let mut chosen = None;
    for c in ranked.iter().take(CANDIDATES) {
        let (base, quote) = split_pair(&c.pair)?;
        let found = format!(
            "  {:<12} okazji {:4} (kup na OKX {}, na Bybit {}), szacunek {:.4}",
            c.pair, c.gaps, c.buy_okx, c.buy_bybit, c.profit
        );
        let quote_fee = match quote_routes.get(&quote) {
            Some(r) => *r,
            None => {
                let r = routes(accounts, &quote).await?;
                quote_routes.insert(quote.clone(), r);
                r
            }
        };
        let base_fee = match routes(accounts, &base).await {
            Ok(r) => r,
            Err(e) => {
                println!("{found} — pomijam: sieci {base} ({e:#})");
                continue;
            }
        };
        let price = prices.get(&(base.clone(), quote.clone())).map(|q| q.bid);
        let cost = match (base_fee, quote_fee, price) {
            ([Some(a), Some(b)], [Some(c), Some(d)], Some(p)) => (a + b) * p + c + d,
            _ => {
                println!("{found} — pomijam: brak wspólnej sieci przelewu");
                continue;
            }
        };
        if cost > route_limit {
            println!("{found} — pomijam: rebalans {cost:.2} {quote}");
            continue;
        }
        println!("{found} — wybrana, rebalans {cost:.3} {quote}");
        chosen = Some(Pick {
            pair: c.pair.clone(),
            base,
            quote,
            base_fee,
            quote_fee,
            profit: c.profit,
        });
        break;
    }
    Ok((ranked, chosen))
}

fn split_pair(pair: &str) -> Result<(String, String)> {
    pair.to_uppercase()
        .split_once('/')
        .map(|(b, q)| (b.to_owned(), q.to_owned()))
        .with_context(|| format!("{pair}: oczekiwano formatu BAZA/KWOTOWANIE"))
}

async fn routes(accounts: &Accounts, asset: &str) -> Result<Routes> {
    let (okx, bybit): (&Okx, &Bybit) = (&accounts.okx, &accounts.bybit);
    Ok(route(
        &okx.networks(asset).await?,
        &bybit.networks(asset).await?,
    ))
}

fn route(okx: &[Network], bybit: &[Network]) -> Routes {
    okx.iter()
        .filter_map(|o| {
            let b = bybit.iter().find(|b| b.key() == o.key())?;
            let open = o.can_withdraw && o.can_deposit && b.can_withdraw && b.can_deposit;
            open.then_some((o.withdraw_fee?, b.withdraw_fee?))
        })
        .min_by(|x, y| (x.0 + x.1).total_cmp(&(y.0 + y.1)))
        .map_or([None, None], |(a, b)| [Some(a), Some(b)])
}
