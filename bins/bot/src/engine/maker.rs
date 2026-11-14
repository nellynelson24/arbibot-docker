use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{Value, json};

use super::maker_live::LiveMaker;
use super::orders::{ceil_to, floor_to, side_id};
use super::taker::books_fresh;
use super::totals::Kind;
use super::{BYBIT, Engine, OKX, REMAINDER_SLIPPAGE, VENUES, clock, price};
use crate::book::{Book, Trade};
use crate::inventory::Wallet;
use crate::ledger::now_ms;
use crate::paper::{Fill, Side};

const HEDGE_SLIPPAGE: f64 = 0.005;
const MAX_FAILED_HEDGES: u32 = 3;
const REST_SLACK_BPS: f64 = 1e-6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Spent {
    Resting,
    Margin,
    Skew,
    Funds,
    Other,
}

impl Spent {
    const ALL: [Spent; 5] = [
        Spent::Resting,
        Spent::Margin,
        Spent::Skew,
        Spent::Funds,
        Spent::Other,
    ];

    pub(crate) fn id(self) -> &'static str {
        match self {
            Spent::Resting => "resting",
            Spent::Margin => "margin",
            Spent::Skew => "skew",
            Spent::Funds => "funds",
            Spent::Other => "other",
        }
    }
}

pub(super) struct Resting {
    pub(super) price: f64,
    pub(super) qty: f64,
    queue_ahead: f64,
    pub(super) id: Option<String>,
}

#[derive(Default)]
pub(super) struct Maker {
    size: f64,
    min_bps: f64,
    requote: Duration,
    rest: bool,
    secs: [[f64; 5]; 2],
    spent_since: [Option<(Spent, Instant)>; 2],
    pub(super) quotes: [Option<Resting>; 2],
    last_quoted: [Option<Instant>; 2],
    pub(super) unhedged: [f64; 2],
    pub(super) unhedged_cash: [f64; 2],
    pub(super) filled_at: [Option<Instant>; 2],
    pub(super) live: Option<LiveMaker>,
}

impl Maker {
    pub(super) fn new(size: f64, min_bps: f64, requote_ms: u64, rest: bool) -> Self {
        Maker {
            size,
            min_bps,
            requote: Duration::from_millis(requote_ms),
            rest,
            ..Maker::default()
        }
    }

    pub(super) fn reset(&mut self) {
        *self = Maker {
            size: self.size,
            min_bps: self.min_bps,
            requote: self.requote,
            rest: self.rest,
            ..Maker::default()
        };
    }

    pub(super) fn spend(&mut self, i: usize, state: Spent, now: Instant) {
        if let Some((last, since)) = self.spent_since[i] {
            self.secs[i][last as usize] += now.saturating_duration_since(since).as_secs_f64();
        }
        self.spent_since[i] = Some((state, now));
    }

    pub(super) fn quote_time(&mut self, now: Instant) -> Value {
        for i in 0..2 {
            if let Some((state, _)) = self.spent_since[i] {
                self.spend(i, state, now);
            }
        }
        let side = |i: usize| -> Value {
            Spent::ALL
                .iter()
                .map(|s| {
                    (
                        s.id().to_owned(),
                        json!((self.secs[i][*s as usize] * 10.0).round() / 10.0),
                    )
                })
                .collect::<serde_json::Map<String, Value>>()
                .into()
        };
        json!({"buy": side(0), "sell": side(1)})
    }
}

pub(super) fn index(side: Side) -> usize {
    match side {
        Side::Buy => 0,
        Side::Sell => 1,
    }
}

fn opposite(side: Side) -> Side {
    match side {
        Side::Buy => Side::Sell,
        Side::Sell => Side::Buy,
    }
}

impl Engine {
    pub(super) fn refresh_quotes(&mut self) {
        let Some(maker) = &self.maker else {
            return;
        };
        let (size, min_bps, requote, rest) = (maker.size, maker.min_bps, maker.requote, maker.rest);
        let skew = self.skew();
        let connected = maker.live.as_ref().is_none_or(|l| l.connected);
        let old = [0, 1].map(|i| maker.quotes[i].as_ref().and_then(|q| q.id.clone()));
        let mut next: [Option<Option<Resting>>; 2] = [None, None];
        let mut placed = [false; 2];
        let mut spent = [Spent::Other; 2];
        let (step, tick) = (self.market.step, self.market.tick[OKX]);
        {
            let okx = self.books[OKX].borrow();
            let bybit = self.books[BYBIT].borrow();
            let fresh = connected && books_fresh([&okx, &bybit], Instant::now());
            for side in [Side::Buy, Side::Sell] {
                let i = index(side);
                let levels = match side {
                    Side::Buy => &okx.bids,
                    Side::Sell => &okx.asks,
                };
                let Some(&(best, best_qty)) = levels.first().filter(|_| fresh) else {
                    next[i] = Some(None);
                    continue;
                };
                let maker = self.maker.as_ref().expect("checked above");
                let can_place = maker.last_quoted[i].is_none_or(|t| t.elapsed() >= requote);
                let threshold = min_bps - skew[i];
                let target = match rest {
                    true => self.rest_target(side, best, size, threshold, &bybit),
                    false => Some((best, floor_to(size / best, step))),
                };
                let mut current = maker.quotes[i]
                    .as_ref()
                    .map(|q| (q.price, q.qty, q.queue_ahead, q.id.clone()));

                if let Some((price, ..)) = current {
                    let moved = match (rest, target) {
                        (true, Some((target, _))) => (target - price).abs() > tick / 2.0,
                        (true, None) => false,
                        (false, _) => match side {
                            Side::Buy => best > price,
                            Side::Sell => best < price,
                        },
                    };
                    if moved && can_place {
                        current = None;
                    }
                }
                let (price, qty) = match (&current, target) {
                    (Some(q), _) => (q.0, floor_to(size / q.0, step)),
                    (None, Some(target)) => target,
                    (None, None) => {
                        next[i] = Some(None);
                        continue;
                    }
                };
                let sized = qty >= self.market.min_qty && qty * price >= self.market.min_notional;
                let slack = if rest { REST_SLACK_BPS } else { 0.0 };
                let reason = idle_reason(
                    self.maker_funded(side, price, qty, &bybit),
                    sized,
                    self.maker_edge(side, price, qty, &bybit),
                    min_bps,
                    threshold - slack,
                );
                next[i] = Some(match (reason, current) {
                    (Some(_), _) => None,
                    (None, Some((price, qty, queue_ahead, id))) => Some(Resting {
                        price,
                        qty,
                        queue_ahead,
                        id,
                    }),
                    (None, None) if can_place => {
                        placed[i] = true;
                        Some(Resting {
                            price,
                            qty,
                            queue_ahead: if rest {
                                level_qty(levels, price)
                            } else {
                                best_qty
                            },
                            id: None,
                        })
                    }
                    (None, None) => None,
                });
                spent[i] = match (reason, &next[i]) {
                    (Some(reason), _) => reason,
                    (None, Some(Some(_))) => Spent::Resting,
                    _ => Spent::Other,
                };
            }
        }
        let maker = self.maker.as_mut().expect("checked above");
        let now = Instant::now();
        for i in 0..2 {
            if let Some(quote) = next[i].take() {
                maker.quotes[i] = quote;
            }
            if placed[i] {
                maker.last_quoted[i] = Some(now);
            }
            maker.spend(i, spent[i], now);
        }
        self.send_quotes(old, placed);
    }

    fn rest_target(
        &self,
        side: Side,
        best: f64,
        size: f64,
        threshold: f64,
        bybit: &Book,
    ) -> Option<(f64, f64)> {
        let fees = (self.market.fees[OKX].maker, self.market.fees[BYBIT].taker);
        let (step, tick) = (self.market.step, self.market.tick[OKX]);
        let hedge_at = |qty: f64| match side {
            Side::Buy => bybit.sell_fill(qty).map(|f| f.0),
            Side::Sell => bybit.buy_fill(qty).map(|f| f.0),
        };
        let first = rest_price(
            side,
            hedge_at(floor_to(size / best, step))?,
            fees,
            threshold,
            best,
            tick,
        );
        let qty = floor_to(size / first, step);
        Some((
            rest_price(side, hedge_at(qty)?, fees, threshold, best, tick),
            qty,
        ))
    }

    fn fill_edge(&self, side: Side, price: f64, qty: f64) -> (Option<f64>, f64) {
        let min_bps = self.maker.as_ref().map_or(0.0, |m| m.min_bps);
        let threshold = min_bps - self.skew()[index(side)];
        let bybit = self.books[BYBIT].borrow();
        (self.maker_edge(side, price, qty, &bybit), threshold)
    }

    fn maker_edge(&self, side: Side, price: f64, qty: f64, bybit: &Book) -> Option<f64> {
        let (maker_fee, taker_fee) = (self.market.fees[OKX].maker, self.market.fees[BYBIT].taker);
        let net = match side {
            Side::Sell => price * (1.0 - maker_fee) / (bybit.buy_fill(qty)?.0 * (1.0 + taker_fee)),
            Side::Buy => bybit.sell_fill(qty)?.0 * (1.0 - taker_fee) / (price * (1.0 + maker_fee)),
        };
        Some((net - 1.0) * 1e4)
    }

    fn maker_funded(&self, side: Side, price: f64, qty: f64, bybit: &Book) -> bool {
        let hedge_fee = self.market.fees[BYBIT].taker;
        let [okx, hedge] = match &self.maker {
            Some(m) => reserve(
                self.inventory.wallets,
                &self.replaced_orders(),
                (m.unhedged, m.unhedged_cash),
                [self.market.fees[OKX].maker, hedge_fee],
            ),
            None => self.inventory.wallets,
        };
        match side {
            Side::Sell => {
                bybit.buy_fill(qty).is_some_and(|(_, worst)| {
                    hedge.quote >= qty * worst * (1.0 + HEDGE_SLIPPAGE) * (1.0 + hedge_fee)
                }) && okx.base >= qty
            }
            Side::Buy => {
                okx.quote >= qty * price * (1.0 + self.market.fees[OKX].maker) && hedge.base >= qty
            }
        }
    }

    pub(super) async fn on_trade(&mut self, trade: Trade) -> Result<()> {
        if self.halted.is_some() || self.live.is_some() {
            return Ok(());
        }
        let Some(maker) = &mut self.maker else {
            return Ok(());
        };
        let mut filled_sides = Vec::new();
        for side in [Side::Buy, Side::Sell] {
            let i = index(side);
            let Some(quote) = &mut maker.quotes[i] else {
                continue;
            };
            let qty = match_trade(quote, side, trade.price, trade.qty);
            if qty <= 0.0 {
                continue;
            }
            let price = quote.price;
            if quote.qty <= f64::EPSILON * qty {
                maker.quotes[i] = None;
            }
            filled_sides.push((side, qty, price));
        }
        for &(side, qty, price) in &filled_sides {
            self.book_maker_fill(
                side,
                qty,
                qty * price,
                ("trade", json!([trade.price, trade.qty])),
            )?;
        }
        for (side, ..) in filled_sides {
            self.hedge(side).await?;
        }
        Ok(())
    }

    pub(super) fn book_maker_fill(
        &mut self,
        side: Side,
        qty: f64,
        value: f64,
        (key, detail): (&str, Value),
    ) -> Result<()> {
        let (expected, threshold) = self.fill_edge(side, value / qty, qty);
        let fee = self.market.fees[OKX].maker;
        let cash = self.inventory.wallets[OKX].book(side, qty, value, fee);
        if let Some(maker) = &mut self.maker {
            let i = index(side);
            maker.unhedged[i] += qty;
            maker.unhedged_cash[i] += cash;
            maker.filled_at[i].get_or_insert_with(Instant::now);
        }
        self.stats.maker_fills += 1;
        let mut line = json!({
            "type": "maker_fill",
            "side": side_id(side),
            "price": value / qty,
            "qty": qty,
            "expected_bps": expected,
            "threshold_bps": threshold,
        });
        line[key] = detail;
        self.ledger.write(line)
    }

    pub(super) async fn hedge(&mut self, side: Side) -> Result<()> {
        let i = index(side);
        let Some(maker) = &self.maker else {
            return Ok(());
        };
        let m = &self.market;
        let qty = floor_to(maker.unhedged[i], m.step);
        let since_fill_ms = maker.filled_at[i].map(|t| t.elapsed().as_secs_f64() * 1e3);
        let hedge_side = opposite(side);
        let (top, book_age_ms) = {
            let book = self.books[BYBIT].borrow();
            let top = match hedge_side {
                Side::Buy => book.asks.first().map(|l| l.0),
                Side::Sell => book.bids.first().map(|l| l.0),
            };
            (top, book.updated.map(|t| t.elapsed().as_secs_f64() * 1e3))
        };
        let Some(top) = top else {
            return Ok(());
        };
        if qty < m.min_qty || qty * top < m.min_notional {
            return Ok(());
        }
        let limit = match hedge_side {
            Side::Buy => top * (1.0 + HEDGE_SLIPPAGE),
            Side::Sell => top * (1.0 - HEDGE_SLIPPAGE),
        };
        let probe = json!({"top": top, "book_age_ms": book_age_ms, "since_fill_ms": since_fill_ms});
        let fill = match self.ioc(BYBIT, hedge_side, qty, limit).await {
            Ok(fill) => fill,
            Err(e) if self.live.is_some() && may_flatten(&e, self.day_failed_hedges) => {
                self.day_failed_hedges += 1;
                return self.flatten(side, qty, format!("{e:#}"), probe).await;
            }
            Err(e) if self.live.is_some() => {
                self.halt_once(format!(
                    "błąd zabezpieczenia makera na Bybit ({e:#}) — sprawdź salda na giełdach"
                ))?;
                self.last_tick_ms = now_ms();
                return self.resync().await;
            }
            Err(e) => return Err(e),
        };
        if fill.qty <= 0.0 {
            self.stats.hedge_misses += 1;
            let mut miss = probe;
            miss["type"] = json!("hedge_miss");
            miss["side"] = json!(side_id(side));
            miss["qty"] = json!(qty);
            miss["orders"] = json!(self.take_orders());
            return self.ledger.write(miss);
        }
        self.book_hedge(side, BYBIT, qty, fill, probe).await
    }

    async fn flatten(&mut self, side: Side, qty: f64, reason: String, probe: Value) -> Result<()> {
        let close = opposite(side);
        let limit = {
            let book = self.books[OKX].borrow();
            match close {
                Side::Sell => book.bids.first().map(|l| l.0 * (1.0 - REMAINDER_SLIPPAGE)),
                Side::Buy => book.asks.first().map(|l| l.0 * (1.0 + REMAINDER_SLIPPAGE)),
            }
        };
        let line = format!(
            "maker: zabezpieczenie na Bybit EU odrzucone ({reason}) — domykam {qty} {} na OKX EEA",
            self.market.base
        );
        println!("[{}] {line}", clock());
        self.tell(line);
        let fill = match limit {
            Some(limit) => self.ioc(OKX, close, qty, limit).await,
            None => Err(anyhow::anyhow!("brak ceny na OKX EEA")),
        };
        match fill {
            Ok(fill) if fill.qty > 0.0 => {
                let mut probe = probe;
                probe["flatten"] = json!(reason);
                self.book_hedge(side, OKX, qty, fill, probe).await
            }
            Ok(_) => Ok(()),
            Err(e) => {
                self.halt_once(format!(
                    "zabezpieczenie makera odrzucone na Bybit ({reason}), a domknięcie na OKX nieudane ({e:#}) — sprawdź salda na giełdach"
                ))?;
                self.last_tick_ms = now_ms();
                self.resync().await
            }
        }
    }

    async fn book_hedge(
        &mut self,
        side: Side,
        venue: usize,
        qty: f64,
        fill: Fill,
        mut detail: Value,
    ) -> Result<()> {
        let i = index(side);
        let close = opposite(side);
        let fee = self.market.fees[venue].taker;
        let cash = self.inventory.wallets[venue].book(close, fill.qty, fill.value(), fee);
        let (step, min_qty, min_notional) = (
            self.market.step,
            self.market.min_qty,
            self.market.min_notional,
        );
        let maker = self.maker.as_mut().expect("checked above");
        let share = (fill.qty / maker.unhedged[i]).min(1.0);
        let maker_cash = maker.unhedged_cash[i] * share;
        maker.unhedged[i] -= fill.qty;
        maker.unhedged_cash[i] -= maker_cash;
        if below_minimum(
            maker.unhedged[i],
            fill.avg_price,
            step,
            (min_qty, min_notional),
        ) {
            maker.filled_at[i] = None;
        }
        let pnl = maker_cash + cash;
        let missed = shortfall(qty, fill.qty, step);
        if missed > 0.0 {
            self.stats.hedge_misses += 1;
        }
        self.stats.maker_hedges += 1;
        self.stats.maker_pnl += pnl;
        self.stats.pnl += pnl;
        self.day_pnl += pnl;
        self.day_trades += 1;
        self.totals.add(Kind::Maker, pnl);
        let notional = maker_cash.abs();
        let line = format!(
            "maker: {} {:.4} {} na OKX EEA, {} na {} po {}: wynik {:+.4} {} ({:+.1} bp)",
            match side {
                Side::Buy => "kupno",
                Side::Sell => "sprzedaż",
            },
            fill.qty,
            self.market.base,
            if venue == OKX {
                "domknięcie"
            } else {
                "zabezpieczenie"
            },
            VENUES[venue].name(),
            price(fill.avg_price),
            pnl,
            self.market.quote,
            pnl / notional * 1e4
        );
        println!("[{}] {line}", clock());
        if self.live.is_some() {
            self.tell(line);
        }
        detail["type"] = json!("maker_hedge");
        detail["side"] = json!(side_id(side));
        detail["qty"] = json!(fill.qty);
        detail["hedge_price"] = json!(fill.avg_price);
        detail["pnl"] = json!(pnl);
        detail["pnl_bps"] = json!(pnl / notional * 1e4);
        detail["missed"] = json!(missed);
        detail["orders"] = json!(self.take_orders());
        self.ledger.write(detail)?;
        if self.live.is_some() {
            self.resync().await?;
        }
        self.last_tick_ms = now_ms();
        self.check_loss_limit()
    }
}

fn idle_reason(
    funded: bool,
    sized: bool,
    edge: Option<f64>,
    min_bps: f64,
    threshold: f64,
) -> Option<Spent> {
    if !funded {
        return Some(Spent::Funds);
    }
    if !sized {
        return Some(Spent::Other);
    }
    match edge {
        None => Some(Spent::Other),
        Some(e) if e >= threshold => None,
        Some(e) if e >= min_bps => Some(Spent::Skew),
        Some(_) => Some(Spent::Margin),
    }
}

pub(super) fn rest_price(
    side: Side,
    hedge: f64,
    (maker_fee, taker_fee): (f64, f64),
    threshold_bps: f64,
    best: f64,
    tick: f64,
) -> f64 {
    let margin = 1.0 + threshold_bps / 1e4;
    match side {
        Side::Buy => floor_to(
            hedge * (1.0 - taker_fee) / ((1.0 + maker_fee) * margin),
            tick,
        )
        .min(best),
        Side::Sell => {
            ceil_to(hedge * (1.0 + taker_fee) * margin / (1.0 - maker_fee), tick).max(best)
        }
    }
}

fn level_qty(levels: &[(f64, f64)], price: f64) -> f64 {
    levels
        .iter()
        .find(|l| (l.0 - price).abs() <= price * 1e-9)
        .map_or(0.0, |l| l.1)
}

fn match_trade(quote: &mut Resting, side: Side, price: f64, qty: f64) -> f64 {
    let tolerance = quote.price * 1e-9;
    let through = match side {
        Side::Sell => price > quote.price + tolerance,
        Side::Buy => price < quote.price - tolerance,
    };
    let reaching = if through {
        qty
    } else if (price - quote.price).abs() <= tolerance {
        let reaching = qty - quote.queue_ahead;
        quote.queue_ahead = (quote.queue_ahead - qty).max(0.0);
        reaching.max(0.0)
    } else {
        0.0
    };
    let filled = reaching.min(quote.qty);
    quote.qty -= filled;
    filled
}

fn may_flatten(e: &anyhow::Error, failed_today: u32) -> bool {
    account::rejected(e) && failed_today < MAX_FAILED_HEDGES
}

pub(super) fn reserve(
    mut w: [Wallet; 2],
    orders: &[(Side, f64, f64)],
    (unhedged, unhedged_cash): ([f64; 2], [f64; 2]),
    [maker_fee, hedge_fee]: [f64; 2],
) -> [Wallet; 2] {
    let hedge_buy = |value: f64| value * (1.0 + HEDGE_SLIPPAGE) * (1.0 + hedge_fee);
    for &(side, open, price) in orders {
        match side {
            Side::Buy => {
                w[OKX].quote -= open * price * (1.0 + maker_fee);
                w[BYBIT].base -= open;
            }
            Side::Sell => {
                w[OKX].base -= open;
                w[BYBIT].quote -= hedge_buy(open * price);
            }
        }
    }
    w[BYBIT].base -= unhedged[index(Side::Buy)];
    w[BYBIT].quote -= hedge_buy(unhedged_cash[index(Side::Sell)].abs());
    w
}

pub(super) fn below_minimum(
    qty: f64,
    price: f64,
    step: f64,
    (min_qty, min_notional): (f64, f64),
) -> bool {
    let qty = floor_to(qty, step);
    qty < min_qty || qty * price < min_notional
}

fn shortfall(qty: f64, filled: f64, step: f64) -> f64 {
    let missed = qty - filled;
    if missed < step / 2.0 { 0.0 } else { missed }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leftovers_too_small_to_hedge_end_the_fill_timer() {
        assert!(below_minimum(0.1, 0.18, 0.1, (1.0, 1.0)));
        assert!(below_minimum(5.0, 0.18, 0.1, (1.0, 1.0)));
        assert!(!below_minimum(20.0, 0.18, 0.1, (1.0, 1.0)));
        assert!(below_minimum(0.06, 0.18, 0.1, (0.1, 1.0)));
    }

    #[test]
    fn rounding_dust_is_not_a_partial_hedge() {
        assert_eq!(shortfall(119.8, 119.8 - 1.4210854715202004e-14, 0.1), 0.0);
        assert_eq!(shortfall(119.8, 119.8, 0.1), 0.0);
        assert!((shortfall(119.8, 119.5, 0.1) - 0.3).abs() < 1e-9);
    }

    #[test]
    fn only_refused_hedges_are_flattened_and_only_a_few_times_a_day() {
        let refused = anyhow::Error::from(account::Rejected("Bybit: kod 170131".to_owned()));
        let unknown = anyhow::anyhow!("Bybit EU: zlecenie bez wyniku po 5 s");
        assert!(may_flatten(&refused, 0));
        assert!(!may_flatten(&refused, MAX_FAILED_HEDGES));
        assert!(!may_flatten(&unknown, 0));
    }

    #[test]
    fn resting_orders_and_pending_fills_reserve_the_hedge_on_bybit() {
        let wallets = [
            Wallet {
                base: 100.0,
                quote: 50.0,
            },
            Wallet {
                base: 281.6,
                quote: 80.0,
            },
        ];
        let orders = [(Side::Buy, 97.2, 0.155), (Side::Sell, 50.0, 0.16)];
        let fees = [0.0008, 0.0025];
        let [okx, bybit] = reserve(wallets, &orders, ([10.0, 0.0], [-1.55, 0.0]), fees);
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        assert!(close(okx.quote, 50.0 - 97.2 * 0.155 * 1.0008));
        assert!(close(okx.base, 50.0));
        assert!(close(bybit.base, 281.6 - 97.2 - 10.0));
        assert!(close(bybit.quote, 80.0 - 50.0 * 0.16 * 1.005 * 1.0025));

        let [_, after_sell] = reserve(wallets, &[], ([0.0, 20.0], [0.0, 3.2]), fees);
        assert!(close(after_sell.quote, 80.0 - 3.2 * 1.005 * 1.0025));
        assert!(close(after_sell.base, 281.6));
    }

    #[test]
    fn a_side_without_an_order_says_why() {
        assert_eq!(idle_reason(true, true, Some(21.0), 20.0, 20.0), None);
        assert_eq!(idle_reason(true, true, Some(12.0), 20.0, 5.0), None);
        assert_eq!(
            idle_reason(true, true, Some(25.0), 20.0, 35.0),
            Some(Spent::Skew)
        );
        assert_eq!(
            idle_reason(true, true, Some(12.0), 20.0, 35.0),
            Some(Spent::Margin)
        );
        assert_eq!(
            idle_reason(false, true, Some(50.0), 20.0, 20.0),
            Some(Spent::Funds)
        );
        assert_eq!(
            idle_reason(true, false, Some(50.0), 20.0, 20.0),
            Some(Spent::Other)
        );
        assert_eq!(
            idle_reason(true, true, None, 20.0, 20.0),
            Some(Spent::Other)
        );
    }

    #[test]
    fn quote_time_adds_up_per_side_and_reason() {
        let mut maker = Maker::new(20.0, 20.0, 1_000, false);
        let t0 = Instant::now();
        maker.spend(0, Spent::Resting, t0);
        maker.spend(1, Spent::Funds, t0);
        maker.spend(0, Spent::Margin, t0 + Duration::from_secs(30));
        maker.spend(0, Spent::Resting, t0 + Duration::from_secs(40));
        let time = maker.quote_time(t0 + Duration::from_secs(100));
        assert_eq!(time["buy"]["resting"], 90.0);
        assert_eq!(time["buy"]["margin"], 10.0);
        assert_eq!(time["sell"]["funds"], 100.0);
        assert_eq!(time["sell"]["skew"], 0.0);
        let again = maker.quote_time(t0 + Duration::from_secs(160));
        assert_eq!(again["buy"]["resting"], 150.0);
    }

    #[test]
    fn resting_price_gives_exactly_the_threshold_and_never_beats_the_best() {
        let fees = (0.0008, 0.0018);
        let edge = |side: Side, price: f64, hedge: f64| match side {
            Side::Buy => (hedge * (1.0 - fees.1) / (price * (1.0 + fees.0)) - 1.0) * 1e4,
            Side::Sell => (price * (1.0 - fees.0) / (hedge * (1.0 + fees.1)) - 1.0) * 1e4,
        };
        let tick = 0.0001;
        let buy = rest_price(Side::Buy, 0.2000, fees, 20.0, 0.2100, tick);
        assert!(edge(Side::Buy, buy, 0.2000) >= 20.0);
        assert!(edge(Side::Buy, buy + tick, 0.2000) < 20.0);
        assert!(buy < 0.2000);
        let sell = rest_price(Side::Sell, 0.2000, fees, 20.0, 0.1900, tick);
        assert!(edge(Side::Sell, sell, 0.2000) >= 20.0);
        assert!(edge(Side::Sell, sell - tick, 0.2000) < 20.0);
        assert!(sell > 0.2000);

        assert_eq!(
            rest_price(Side::Buy, 0.2000, fees, 20.0, 0.1950, tick),
            0.1950
        );
        assert_eq!(
            rest_price(Side::Sell, 0.2000, fees, 20.0, 0.2050, tick),
            0.2050
        );
        let skewed = rest_price(Side::Buy, 0.2000, fees, 5.0, 0.2100, tick);
        assert!(skewed > buy);
    }

    #[test]
    fn queue_ahead_is_the_size_already_at_our_price() {
        let bids = [(0.2010, 500.0), (0.2005, 300.0), (0.2000, 900.0)];
        assert_eq!(level_qty(&bids, 0.2005), 300.0);
        assert_eq!(level_qty(&bids, 0.1990), 0.0);
    }

    fn resting(qty: f64, queue_ahead: f64) -> Resting {
        Resting {
            price: 1.0,
            qty,
            queue_ahead,
            id: None,
        }
    }

    #[test]
    fn maker_order_fills_after_the_queue_or_by_a_trade_through() {
        let mut sell = resting(10.0, 5.0);

        assert_eq!(match_trade(&mut sell, Side::Sell, 1.0, 3.0), 0.0);
        assert_eq!(match_trade(&mut sell, Side::Sell, 1.0, 4.0), 2.0);

        assert_eq!(match_trade(&mut sell, Side::Sell, 1.01, 20.0), 8.0);
        assert_eq!(sell.qty, 0.0);

        assert_eq!(
            match_trade(&mut resting(10.0, 0.0), Side::Sell, 0.99, 5.0),
            0.0
        );
        assert_eq!(
            match_trade(&mut resting(10.0, 0.0), Side::Buy, 0.99, 4.0),
            4.0
        );
    }
}
