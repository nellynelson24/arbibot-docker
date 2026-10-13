use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::json;
use venues::Venue;

use crate::clock::now_ms;
use crate::engine::{JsonlLog, MAX_PLAUSIBLE_GAP, Top, round2};
use crate::feed::TradeUpdate;
use crate::universe::{PairId, Universe};

const BUCKETS: f64 = 4.0;
const PRICE_EPS: f64 = 1e-9;

pub struct Config {
    pub capital: f64,
    pub min_edge_bps: f64,
    pub requote: Duration,
    pub hedge_depth_factor: f64,
    pub min_order: f64,
    pub hedge_delay: Duration,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Market {
    pair: PairId,
    maker: Venue,
    hedge: Venue,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Buy,
    Sell,
}

impl Side {
    const BOTH: [Side; 2] = [Side::Buy, Side::Sell];

    fn index(self) -> usize {
        self as usize
    }

    fn id(self) -> &'static str {
        match self {
            Side::Buy => "buy",
            Side::Sell => "sell",
        }
    }
}

struct Quote {
    price: f64,
    remaining: f64,
    queue_ahead: f64,
    placed: Instant,
}

#[derive(Clone, Copy)]
struct Balance {
    quote: f64,
    base: f64,
}

struct MarketState {
    balances: [Balance; 2],
    quotes: [Option<Quote>; 2],
    last_placed: [Option<Instant>; 2],
}

struct PendingHedge {
    market: Market,
    side: Side,
    price: f64,
    size: f64,
    wait_ms: f64,
    due: Instant,
}

#[derive(Default)]
pub struct Window {
    pub fills: u32,
    pub profit: f64,
    pub unhedged: u32,
}

pub struct MakerSim<'a> {
    universe: &'a Universe,
    cfg: Config,
    markets: HashMap<Market, MarketState>,
    pending: Vec<PendingHedge>,
    log: JsonlLog,
    pub window: Window,
}

impl<'a> MakerSim<'a> {
    pub fn new(universe: &'a Universe, cfg: Config, data_dir: &Path) -> Result<Self> {
        Ok(MakerSim {
            universe,
            cfg,
            markets: HashMap::new(),
            pending: Vec::new(),
            log: JsonlLog::new(data_dir.join("maker"))?,
            window: Window::default(),
        })
    }

    pub fn on_top(
        &mut self,
        pair: PairId,
        venue: Venue,
        tops: &[Top; Venue::COUNT],
        live: &[bool; Venue::COUNT],
    ) {
        for other in Venue::ALL {
            if other == venue || !live[other.index()] {
                continue;
            }
            for (maker, hedge) in [(venue, other), (other, venue)] {
                self.refresh(Market { pair, maker, hedge }, tops);
            }
        }
    }

    pub fn on_trade(&mut self, trade: &TradeUpdate, live: &[bool; Venue::COUNT]) -> Result<()> {
        let TradeUpdate {
            pair,
            venue,
            price,
            qty,
            taker_buys,
        } = *trade;

        let side = if taker_buys { Side::Sell } else { Side::Buy };
        for hedge in Venue::ALL {
            if hedge == venue || !live[hedge.index()] {
                continue;
            }
            let market = Market {
                pair,
                maker: venue,
                hedge,
            };
            let Some(quote) = self
                .markets
                .get_mut(&market)
                .and_then(|m| m.quotes[side.index()].as_mut())
            else {
                continue;
            };
            let filled = match_trade(quote, side, price, qty);
            if filled > 0.0 {
                self.fill(market, side, filled);
            }
        }
        Ok(())
    }

    pub fn log_status(&mut self, every: Duration) -> Result<()> {
        let now = now_ms();
        let line = json!({"type": "status", "ts_ms": now as u64, "interval_secs": every.as_secs()});
        self.log.write(now, &line)
    }

    pub fn tick(&mut self, tops: &[[Top; Venue::COUNT]]) -> Result<()> {
        let now = Instant::now();
        let (due, waiting): (Vec<PendingHedge>, Vec<PendingHedge>) =
            self.pending.drain(..).partition(|p| p.due <= now);
        self.pending = waiting;
        for hedge in due {
            let top = &tops[hedge.market.pair][hedge.market.hedge.index()];
            self.hedge(&hedge, top)?;
        }
        Ok(())
    }

    pub fn reset(&mut self) {
        for state in self.markets.values_mut() {
            state.quotes = [None, None];
        }
        self.pending.clear();
    }

    pub fn venue_down(&mut self, venue: Venue) {
        for (market, state) in &mut self.markets {
            if market.maker == venue || market.hedge == venue {
                state.quotes = [None, None];
            }
        }
    }

    fn refresh(&mut self, market: Market, tops: &[Top; Venue::COUNT]) {
        let maker = &tops[market.maker.index()];
        let hedge = &tops[market.hedge.index()];
        let usable = maker.is_usable()
            && hedge.is_usable()
            && (maker.mid() / hedge.mid() - 1.0).abs() <= MAX_PLAUSIBLE_GAP;
        if !usable {
            if let Some(state) = self.markets.get_mut(&market) {
                state.quotes = [None, None];
            }
            return;
        }
        let capital = self.cfg.capital;
        let state = self
            .markets
            .entry(market)
            .or_insert_with(|| MarketState::new(capital));
        for side in Side::BOTH {
            state.update_quote(side, market, maker, hedge, &self.cfg);
        }
    }

    fn fill(&mut self, market: Market, side: Side, size: f64) {
        let Some(state) = self.markets.get_mut(&market) else {
            return;
        };
        let Some(quote) = &state.quotes[side.index()] else {
            return;
        };
        let (price, wait_ms) = (quote.price, quote.placed.elapsed().as_secs_f64() * 1000.0);
        if quote.remaining <= f64::EPSILON * size {
            state.quotes[side.index()] = None;
        }
        let [maker_balance, hedge_balance] = &mut state.balances;
        match side {
            Side::Sell => {
                maker_balance.base -= size;
                maker_balance.quote += size;
                hedge_balance.quote -= size;
                hedge_balance.base += size;
            }
            Side::Buy => {
                maker_balance.quote -= size;
                maker_balance.base += size;
                hedge_balance.base -= size;
                hedge_balance.quote += size;
            }
        }
        self.pending.push(PendingHedge {
            market,
            side,
            price,
            size,
            wait_ms,
            due: Instant::now() + self.cfg.hedge_delay,
        });
    }

    fn hedge(&mut self, fill: &PendingHedge, hedge: &Top) -> Result<()> {
        let PendingHedge {
            market,
            side,
            price,
            size,
            wait_ms,
            ..
        } = *fill;
        let (maker_fee, taker_fee) = (market.maker.maker_fee(), market.hedge.taker_fee());
        let qty = size / price;
        let (hedge_price, profit) = match side {
            Side::Sell => (
                hedge.ask,
                qty * price * (1.0 - maker_fee) - qty * hedge.ask * (1.0 + taker_fee),
            ),
            Side::Buy => (
                hedge.bid,
                qty * hedge.bid * (1.0 - taker_fee) - qty * price * (1.0 + maker_fee),
            ),
        };

        let profit = profit.is_finite().then_some(profit);
        if profit.is_none() {
            self.window.unhedged += 1;
        }

        self.window.fills += 1;
        self.window.profit += profit.unwrap_or(0.0);
        let now = now_ms();
        let line = json!({
            "type": "fill",
            "ts_ms": now as u64,
            "pair": self.universe.pairs[market.pair].to_string(),
            "maker": market.maker.id(),
            "hedge": market.hedge.id(),
            "side": side.id(),
            "price": price,
            "hedge_price": hedge_price,
            "size": round2(size),
            "profit": profit,
            "unhedged": profit.is_none(),
            "edge_bps": profit.map(|p| round2(p / size * 1e4)),
            "wait_ms": round2(wait_ms),
            "hedge_delay_ms": self.cfg.hedge_delay.as_millis() as u64,
            "capital": self.cfg.capital,
        });
        self.log.write(now, &line)
    }
}

impl MarketState {
    fn new(capital: f64) -> Self {
        let bucket = Balance {
            quote: capital / BUCKETS,
            base: capital / BUCKETS,
        };
        MarketState {
            balances: [bucket; 2],
            quotes: [None, None],
            last_placed: [None, None],
        }
    }

    fn update_quote(&mut self, side: Side, market: Market, maker: &Top, hedge: &Top, cfg: &Config) {
        let now = Instant::now();
        let size = self.order_size(side, hedge, cfg);
        let can_place = self.last_placed[side.index()]
            .is_none_or(|last| now.duration_since(last) >= cfg.requote);
        let (best, best_qty) = match side {
            Side::Sell => (maker.ask, maker.ask_qty),
            Side::Buy => (maker.bid, maker.bid_qty),
        };

        let slot = &mut self.quotes[side.index()];
        if let Some(quote) = slot {
            let ahead_of_us = match side {
                Side::Sell => best < quote.price,
                Side::Buy => best > quote.price,
            };
            if ahead_of_us && can_place {
                *slot = None;
            }
        }

        let price = slot.as_ref().map_or(best, |q| q.price);
        let edge_bps = edge(side, market, price, hedge) * 1e4;
        if !(edge_bps >= cfg.min_edge_bps && size >= cfg.min_order) {
            *slot = None;
            return;
        }
        if slot.is_none() && can_place {
            *slot = Some(Quote {
                price: best,
                remaining: size,
                queue_ahead: if best_qty.is_finite() { best_qty } else { 0.0 },
                placed: now,
            });
            self.last_placed[side.index()] = Some(now);
        }
    }

    fn order_size(&self, side: Side, hedge: &Top, cfg: &Config) -> f64 {
        let [maker, hedge_balance] = self.balances;
        let (inventory, hedge_inventory, hedge_offer) = match side {
            Side::Sell => (maker.base, hedge_balance.quote, hedge.ask_qty * hedge.ask),
            Side::Buy => (maker.quote, hedge_balance.base, hedge.bid_qty * hedge.bid),
        };
        if !hedge_offer.is_finite() {
            return 0.0;
        }
        (cfg.capital / BUCKETS)
            .min(inventory)
            .min(hedge_inventory)
            .min(hedge_offer * cfg.hedge_depth_factor)
    }
}

fn edge(side: Side, market: Market, price: f64, hedge: &Top) -> f64 {
    let (maker_fee, taker_fee) = (market.maker.maker_fee(), market.hedge.taker_fee());
    match side {
        Side::Sell => price * (1.0 - maker_fee) / (hedge.ask * (1.0 + taker_fee)) - 1.0,
        Side::Buy => hedge.bid * (1.0 - taker_fee) / (price * (1.0 + maker_fee)) - 1.0,
    }
}

fn match_trade(quote: &mut Quote, side: Side, price: f64, qty: f64) -> f64 {
    let tolerance = quote.price * PRICE_EPS;
    let through = match side {
        Side::Sell => price > quote.price + tolerance,
        Side::Buy => price < quote.price - tolerance,
    };
    let filled_base = if through {
        qty
    } else if (price - quote.price).abs() <= tolerance {
        let reaching_us = qty - quote.queue_ahead;
        quote.queue_ahead = (quote.queue_ahead - qty).max(0.0);
        reaching_us.max(0.0)
    } else {
        0.0
    };
    let filled = (filled_base * quote.price).min(quote.remaining);
    quote.remaining -= filled;
    filled
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quote(price: f64, queue_ahead: f64) -> Quote {
        Quote {
            price,
            remaining: 100.0,
            queue_ahead,
            placed: Instant::now(),
        }
    }

    #[test]
    fn trade_at_our_price_first_eats_the_queue() {
        let mut q = quote(2.0, 30.0);

        assert_eq!(match_trade(&mut q, Side::Sell, 2.0, 20.0), 0.0);

        assert_eq!(match_trade(&mut q, Side::Sell, 2.0, 25.0), 30.0);
        assert_eq!(q.remaining, 70.0);
    }

    #[test]
    fn trade_through_our_price_fills_us_up_to_remaining() {
        let mut q = quote(2.0, 1_000.0);
        assert_eq!(match_trade(&mut q, Side::Sell, 2.1, 80.0), 100.0);
        assert_eq!(q.remaining, 0.0);
    }

    #[test]
    fn trade_on_the_wrong_side_of_our_price_does_nothing() {
        let mut q = quote(2.0, 0.0);
        assert_eq!(match_trade(&mut q, Side::Sell, 1.9, 50.0), 0.0);
        assert_eq!(match_trade(&mut q, Side::Buy, 2.1, 50.0), 0.0);
    }

    #[test]
    fn edge_counts_maker_and_taker_fees() {
        let market = Market {
            pair: 0,
            maker: Venue::Okx,
            hedge: Venue::Bybit,
        };
        let hedge = Top {
            bid: 0.99,
            bid_qty: 1.0,
            ask: 1.0,
            ask_qty: 1.0,
        };
        let sell = edge(Side::Sell, market, 1.01, &hedge);
        let expected =
            1.01 * (1.0 - Venue::Okx.maker_fee()) / (1.0 * (1.0 + Venue::Bybit.taker_fee())) - 1.0;
        assert!((sell - expected).abs() < 1e-12);
    }
}
