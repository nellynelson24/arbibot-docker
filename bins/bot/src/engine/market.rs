use std::collections::HashMap;
use std::path::Path;

use account::{Bybit, Fees, Network, Okx};
use anyhow::{Context, Result, bail};
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

#[derive(Clone, Copy)]
pub(super) struct Selection {
    pub(super) size: f64,
    pub(super) min_bps: f64,
    pub(super) route_limit: f64,
    pub(super) maker: Option<(f64, f64)>,
    pub(super) any_route: bool,
}

impl Selection {
    pub(super) fn of(args: &Args) -> Selection {
        Selection {
            size: args.size,
            min_bps: args.min_bps,
            route_limit: MAX_ROUTE_COST * args.capital,
            maker: args
                .switch_maker_hours
                .map(|h| (h, args.maker_size.unwrap_or(args.size))),
            any_route: args.switch_any_route,
        }
    }
}

pub(super) fn per_review(profit: f64, hours: f64) -> f64 {
    profit * f64::from(REVIEW_DAYS) * 24.0 / hours
}

pub(super) async fn load_market(
    args: &Args,
    http: &reqwest::Client,
    accounts: &Accounts,
) -> Result<Market> {
    let selection = Selection::of(args);
    let fixed = args.pair.as_deref().map(split_pair).transpose()?;
    let pick = match (&fixed, selection.maker) {
        (Some(_), None) => None,
        _ => {
            let quote = fixed.as_ref().map(|(_, q)| q.as_str());
            match picks(&args.data_dir, &selection, quote, http, accounts).await {
                Ok((_, pick)) => pick,
                Err(e) if fixed.is_some() => {
                    println!("Wybór pary nieudany ({e:#}); para zapasowa z --pair.");
                    None
                }
                Err(e) => return Err(e),
            }
        }
    };
    let (base, quote, base_fee, quote_fee) = match (pick, fixed) {
        (Some(pick), _) => (pick.base, pick.quote, pick.base_fee, pick.quote_fee),
        (None, Some((base, quote))) => {
            let base_fee = routes(accounts, &base).await?;
            let quote_fee = routes(accounts, &quote).await?;
            (base, quote, base_fee, quote_fee)
        }
        (None, None) => bail!(
            "brak pary OKX↔Bybit spełniającej warunki wyboru w danych skanera ({}) — podaj parę: --pair STRK/USDC",
            match selection.maker {
                Some((hours, _)) => format!("symulacja makera z ostatnich {hours} h"),
                None => format!(
                    "okazje ≥ {} bp i tani rebalans z ostatnich {REVIEW_DAYS} dni",
                    args.min_bps
                ),
            }
        ),
    };
    load_pair(accounts, (base, quote), base_fee, quote_fee, args.fee_scale).await
}

fn scaled(fees: Fees, scale: f64) -> Fees {
    Fees {
        maker: fees.maker * scale,
        taker: fees.taker * scale,
    }
}

pub(super) async fn load_pair(
    accounts: &Accounts,
    (base, quote): (String, String),
    base_fee: Routes,
    quote_fee: Routes,
    fee_scale: f64,
) -> Result<Market> {
    let (okx, bybit) = (&accounts.okx, &accounts.bybit);
    let fees = [
        okx.fees(&base, &quote).await.context("prowizje OKX")?,
        bybit.fees(&base, &quote).await.context("prowizje Bybit")?,
    ]
    .map(|f| scaled(f, fee_scale));
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
    sel: &Selection,
    quote: Option<&str>,
    http: &reqwest::Client,
    accounts: &Accounts,
) -> Result<(Vec<selector::Candidate>, Option<Pick>)> {
    let fee_bps = (Venue::Okx.taker_fee() + Venue::Bybit.taker_fee()) * 1e4;
    let ranked = match sel.maker {
        Some((hours, size)) => {
            let mut ranked = selector::rank_maker(data_dir, now_ms(), hours, size);
            for c in &mut ranked {
                c.profit = per_review(c.profit, hours);
            }
            ranked
        }
        None => selector::rank(
            data_dir,
            now_ms(),
            REVIEW_DAYS,
            fee_bps,
            sel.min_bps,
            sel.size,
        ),
    };
    if ranked.is_empty() {
        return Ok((ranked, None));
    }

    let books = tokio::try_join!(
        Venue::Okx.fetch_quotes(http),
        Venue::Bybit.fetch_quotes(http)
    )
    .context("ceny giełd")?;
    let books = [books.0, books.1];
    let route_limit = sel.route_limit;
    let route_rule = match sel.any_route {
        true => "bez wymogu trasy przelewu".to_owned(),
        false => format!("limit rebalansu {route_limit:.2}"),
    };
    match sel.maker {
        Some((hours, size)) => println!(
            "Selektor (symulacja makera OKX → Bybit z ostatnich {hours} h, zlecenie {size}, \
             zysk w przeliczeniu na {REVIEW_DAYS} dni; {route_rule}):"
        ),
        None => println!(
            "Selektor (ostatnie {REVIEW_DAYS} dni, okazje ≥ {} bp po prowizjach {fee_bps:.0} bp, \
             transakcja {}; {route_rule}):",
            sel.min_bps, sel.size
        ),
    }
    let mut quote_routes: HashMap<String, Routes> = HashMap::new();
    let mut chosen = None;
    for (c, issue) in selector::screened(&ranked, &books, quote, CANDIDATES) {
        let (base, quote) = split_pair(&c.pair)?;
        let found = match sel.maker {
            Some(_) => format!(
                "  {:<12} wypełnień {:4} (kupno {}, sprzedaż {}), szacunek {:.4}",
                c.pair, c.gaps, c.buy_okx, c.buy_bybit, c.profit
            ),
            None => format!(
                "  {:<12} okazji {:4} (kup na OKX {}, na Bybit {}), szacunek {:.4}",
                c.pair, c.gaps, c.buy_okx, c.buy_bybit, c.profit
            ),
        };
        if let Some(reason) = issue {
            println!("{found} — pomijam: {reason}");
            continue;
        }
        let quote_fee = match quote_routes.get(&quote) {
            Some(r) => *r,
            None => {
                let r = match routes(accounts, &quote).await {
                    Ok(r) => r,
                    Err(_) if sel.any_route => [None, None],
                    Err(e) => return Err(e),
                };
                quote_routes.insert(quote.clone(), r);
                r
            }
        };
        let base_fee = match routes(accounts, &base).await {
            Ok(r) => r,
            Err(_) if sel.any_route => [None, None],
            Err(e) => {
                println!("{found} — pomijam: sieci {base} ({e:#})");
                continue;
            }
        };
        let price = books[0].get(&(base.clone(), quote.clone())).map(|q| q.bid);
        let cost = match (base_fee, quote_fee, price) {
            ([Some(a), Some(b)], [Some(c), Some(d)], Some(p)) => Some((a + b) * p + c + d),
            _ => None,
        };
        match (cost, sel.any_route) {
            (None, false) => {
                println!("{found} — pomijam: brak wspólnej sieci przelewu");
                continue;
            }
            (Some(cost), false) if cost > route_limit => {
                println!("{found} — pomijam: rebalans {cost:.2} {quote}");
                continue;
            }
            (Some(cost), _) => println!("{found} — wybrana, rebalans {cost:.3} {quote}"),
            (None, true) => {
                println!("{found} — wybrana, bez trasy przelewu (salda wyrównuje handel)")
            }
        }
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
    cheapest(okx, bybit).map_or([None, None], |r| [Some(r.fees[0]), Some(r.fees[1])])
}

#[derive(Clone, Debug)]
pub(super) struct Route {
    pub(super) chains: [String; 2],
    pub(super) fees: [f64; 2],
    pub(super) min: [f64; 2],
    pub(super) decimals: [u32; 2],
}

pub(super) async fn route_of(accounts: &Accounts, asset: &str) -> Result<Option<Route>> {
    Ok(cheapest(
        &accounts.okx.networks(asset).await?,
        &accounts.bybit.networks(asset).await?,
    ))
}

fn cheapest(okx: &[Network], bybit: &[Network]) -> Option<Route> {
    okx.iter()
        .filter_map(|o| {
            let b = bybit.iter().find(|b| b.key() == o.key())?;
            let open = o.can_withdraw && o.can_deposit && b.can_withdraw && b.can_deposit;
            open.then_some(())?;
            Some(Route {
                chains: [o.chain.clone(), b.chain.clone()],
                fees: [o.withdraw_fee?, b.withdraw_fee?],
                min: [o.withdraw_min.unwrap_or(0.0), b.withdraw_min.unwrap_or(0.0)],
                decimals: [
                    o.withdraw_decimals.unwrap_or(6),
                    b.withdraw_decimals.unwrap_or(6),
                ],
            })
        })
        .min_by(|x, y| (x.fees[0] + x.fees[1]).total_cmp(&(y.fees[0] + y.fees[1])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fee_scale_multiplies_maker_and_taker() {
        let half = scaled(
            Fees {
                maker: 0.0008,
                taker: 0.0025,
            },
            0.5,
        );
        assert!((half.maker - 0.0004).abs() < 1e-12);
        assert!((half.taker - 0.00125).abs() < 1e-12);
    }
}
