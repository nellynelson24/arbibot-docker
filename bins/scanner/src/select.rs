use anyhow::{Context, Result};
use futures_util::future::join_all;
use reqwest::Client;
use venues::{Book, Venue};

use crate::engine::{Route, Signal, Top, best_route};
use crate::universe::{PairId, Universe};

const MIN_OFFER: f64 = 10.0;

pub async fn top_pairs(
    client: &Client,
    universe: &Universe,
    venues: &[Venue],
    limit: usize,
) -> Vec<(Route, Signal)> {
    let fetched = join_all(venues.iter().map(|v| v.fetch_quotes(client))).await;
    let books: Vec<(Venue, Book)> = venues
        .iter()
        .copied()
        .zip(fetched)
        .filter_map(|(venue, book)| book.ok().map(|b| (venue, b)))
        .collect();

    let mut picks: Vec<(Route, Signal)> = universe
        .pairs
        .iter()
        .enumerate()
        .filter_map(|(pair, p)| {
            let key = (p.base.clone(), p.quote.clone());
            let mut tops = [Top::UNKNOWN; Venue::COUNT];
            for (venue, book) in &books {
                if let Some(quote) = book.get(&key) {
                    tops[venue.index()] = Top::from_quote(quote);
                }
            }
            best_route(pair, &tops, |_| true)
        })
        .filter(|(_, signal)| signal.size.is_none_or(|s| s >= MIN_OFFER))
        .collect();
    picks.sort_by(|a, b| b.1.net_bps.total_cmp(&a.1.net_bps));
    picks.truncate(limit);
    picks
}

pub fn named_pairs(universe: &Universe, names: &[String]) -> Result<Vec<PairId>> {
    names
        .iter()
        .map(|name| {
            let name = name.to_uppercase();
            universe
                .pairs
                .iter()
                .position(|p| p.to_string() == name)
                .with_context(|| format!("{name}: brak tej pary na co najmniej 2 giełdach"))
        })
        .collect()
}
