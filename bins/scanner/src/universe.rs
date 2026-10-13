use std::collections::BTreeMap;
use std::fmt;

use anyhow::Result;
use futures_util::future::join_all;
use reqwest::Client;
use venues::Venue;

use crate::feed;

pub type PairId = usize;

pub struct Pair {
    pub base: String,
    pub quote: String,
}

impl fmt::Display for Pair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.base, self.quote)
    }
}

#[derive(Clone)]
pub struct Listing {
    pub pair: PairId,
    pub symbol: String,
    pub key: (String, String),
}

pub struct Universe {
    pub pairs: Vec<Pair>,
    listings: [Vec<Listing>; Venue::COUNT],
}

impl Universe {
    pub async fn discover(client: &Client, venues: &[Venue], quotes: &[String]) -> Result<Self> {
        let books = join_all(venues.iter().map(|v| v.fetch_quotes(client))).await;
        let mut listed: BTreeMap<(String, String), Vec<Venue>> = BTreeMap::new();
        for (&venue, book) in venues.iter().zip(books) {
            match book {
                Ok(book) => {
                    for key in book.into_keys().filter(|(_, quote)| quotes.contains(quote)) {
                        listed.entry(key).or_default().push(venue);
                    }
                }
                Err(e) => eprintln!("{}: pominięta, brak listy rynków ({e:#})", venue.name()),
            }
        }

        let mut universe = Universe {
            pairs: Vec::new(),
            listings: Default::default(),
        };
        for ((base, quote), on) in listed {
            if on.len() < 2 {
                continue;
            }
            let pair = universe.pairs.len();
            for venue in on {
                universe.listings[venue.index()].push(Listing {
                    pair,
                    symbol: feed::ws_symbol(venue, &base, &quote),
                    key: (base.clone(), quote.clone()),
                });
            }
            universe.pairs.push(Pair { base, quote });
        }
        Ok(universe)
    }

    pub fn listings(&self, venue: Venue) -> &[Listing] {
        &self.listings[venue.index()]
    }
}
