use anyhow::Result;
use serde_json::json;

use super::orders::floor_to;
use std::time::Instant;

use super::{BYBIT, Engine, Plan, REMAINDER_SLIPPAGE, SIZE_MARGIN, VENUES, clock};
use crate::book::Book;
use crate::ledger::now_ms;
use crate::paper::{Fill, Side};

const MIN_TRADE_SHARE: f64 = 0.25;

impl Engine {
    pub(super) async fn taker(&mut self) -> Result<()> {
        let thresholds = self.thresholds();
        for d in 0..2 {
            let Some(plan) = self.plan_sized(d, self.size) else {
                continue;
            };
            if plan.expected_bps < thresholds[d] {
                self.armed[d] = true;
                continue;
            }
            if !self.armed[d] {
                continue;
            }
            self.armed[d] = false;
            self.stats.signals += 1;
            if !self.has_funds(d, &plan) {
                self.stats.no_funds += 1;
                self.ledger.write(json!({
                    "type": "skip",
                    "reason": "no_funds",
                    "buy": VENUES[d].id(),
                    "sell": VENUES[1 - d].id(),
                    "expected_bps": plan.expected_bps,
                }))?;
                continue;
            }
            return self.trade(d, plan, false).await;
        }
        Ok(())
    }

    pub(super) fn plan_sized(&self, d: usize, size: f64) -> Option<Plan> {
        let buy_book = self.books[d].borrow();
        let sell_book = self.books[1 - d].borrow();
        if !books_fresh([&buy_book, &sell_book], Instant::now()) {
            return None;
        }
        let m = &self.market;
        let ask = buy_book.asks.first()?.0;

        let w = self.free();
        let full = size / ask;
        let affordable = (w[d].quote / (ask * (1.0 + m.fees[d].taker) * (1.0 + SIZE_MARGIN)))
            .min(w[1 - d].base)
            .min(full);
        let qty = if affordable * ask >= self.min_trade() {
            floor_to(affordable, m.step)
        } else {
            floor_to(full, m.step)
        };
        if qty < m.min_qty || qty * ask < m.min_notional {
            return None;
        }
        let (buy_avg, buy_limit) = buy_book.buy_fill(qty)?;
        let (sell_avg, sell_limit) = sell_book.sell_fill(qty)?;
        let (fee_buy, fee_sell) = (m.fees[d].taker, m.fees[1 - d].taker);
        let net = sell_avg * (1.0 - fee_sell) / (buy_avg * (1.0 + fee_buy)) - 1.0;

        let slack = self.slack_bps.min(net * 1e4 / 4.0).max(0.0) / 1e4;
        Some(Plan {
            qty,
            buy_limit: buy_limit * (1.0 + slack),
            sell_limit: sell_limit * (1.0 - slack),
            expected_bps: net * 1e4,
        })
    }

    pub(super) fn min_trade(&self) -> f64 {
        (self.size * MIN_TRADE_SHARE).max(self.market.min_notional)
    }

    pub(super) fn has_funds(&self, d: usize, plan: &Plan) -> bool {
        let w = self.free();
        w[d].quote >= plan.qty * plan.buy_limit * (1.0 + self.market.fees[d].taker)
            && w[1 - d].base >= plan.qty
    }

    pub(super) async fn trade(&mut self, d: usize, plan: Plan, rebalancing: bool) -> Result<()> {
        let (b, s) = (d, 1 - d);
        let (fee_b, fee_s) = (self.market.fees[b].taker, self.market.fees[s].taker);
        let first = (self.sequential && !rebalancing).then(|| self.first_leg(b, s));
        let (buy, sell) = match first {
            Some(v) => self.legs_in_turn(b, s, v, &plan).await,
            None => {
                tokio::join!(
                    self.ioc(b, Side::Buy, plan.qty, plan.buy_limit),
                    self.ioc(s, Side::Sell, plan.qty, plan.sell_limit),
                )
            }
        };
        let (buy, sell) = match (buy, sell) {
            (Ok(buy), Ok(sell)) => (buy, sell),
            (buy, sell) => {
                let errors: Vec<String> = [buy.err(), sell.err()]
                    .into_iter()
                    .flatten()
                    .map(|e| format!("{e:#}"))
                    .collect();
                self.halt(format!(
                    "błąd zlecenia ({}) — sprawdź salda na giełdach",
                    errors.join("; ")
                ))?;
                self.last_tick_ms = now_ms();
                return self.resync().await;
            }
        };
        let wallets = &mut self.inventory.wallets;
        let bought = wallets[b].book(Side::Buy, buy.qty, buy.value(), fee_b);
        let sold = wallets[s].book(Side::Sell, sell.qty, sell.value(), fee_s);
        let mut pnl = sold + bought;

        let excess = buy.qty - sell.qty;
        let mut remainder = Fill::default();
        let mut remainder_venue = None;
        if excess.abs() >= self.market.step / 2.0 {
            match self.close_remainder(excess).await {
                Ok(Some((v, fill, cash))) => {
                    remainder = fill;
                    remainder_venue = Some(v);
                    pnl += cash;
                }
                Ok(None) => {}
                Err(e) => self.halt(format!(
                    "błąd domykania reszty ({e:#}) — sprawdź salda na giełdach"
                ))?,
            }
        }
        let open = excess.abs() - remainder.qty;

        if open > 0.0
            && let Some(mid) = self.books[b].borrow().mid()
        {
            pnl += excess.signum() * open * mid;
        }

        let outcome = if buy.qty == 0.0 && sell.qty == 0.0 {
            "brak wypełnienia"
        } else if excess.abs() < self.market.step / 2.0 {
            "sparowana"
        } else {
            "z resztą"
        };
        if rebalancing {
            self.stats.rebalance_trades += 1;
            self.stats.rebalance_trade_pnl += pnl;
        } else {
            self.stats.trades += 1;
            self.day_trades += 1;
            match outcome {
                "brak wypełnienia" => self.stats.unfilled += 1,
                "sparowana" => self.stats.matched += 1,
                _ => self.stats.with_remainder += 1,
            }
        }
        self.stats.pnl += pnl;
        self.day_pnl += pnl;
        let notional = plan.qty * plan.buy_limit;
        let mut notes = String::new();
        if let Some(v) = remainder_venue {
            notes += &format!(", reszta domknięta na {}", VENUES[v].name());
        }
        if open > self.market.step / 2.0 {
            notes += &format!(", niezamknięte {open:.4}");
        }
        let line = format!(
            "{}kup {} → sprzedaj {} {:.4} {}: oczekiwane {:+.1} bp, wynik {:+.4} {} ({:+.1} bp), {outcome}{notes}",
            if rebalancing {
                "rebalans handlem: "
            } else {
                ""
            },
            VENUES[b].name(),
            VENUES[s].name(),
            plan.qty,
            self.market.base,
            plan.expected_bps,
            pnl,
            self.market.quote,
            pnl / notional * 1e4,
        );
        println!("[{}] {line}", clock());
        self.tell(line);
        self.ledger.write(json!({
            "type": if rebalancing { "rebalance_trade" } else { "trade" },
            "buy": VENUES[b].id(),
            "sell": VENUES[s].id(),
            "qty": plan.qty,
            "expected_bps": plan.expected_bps,
            "buy_fill": [buy.qty, buy.avg_price],
            "sell_fill": [sell.qty, sell.avg_price],
            "remainder_fill": [remainder.qty, remainder.avg_price],
            "remainder_venue": remainder_venue.map(|v| VENUES[v].id()),
            "limits": [plan.buy_limit, plan.sell_limit],
            "first": first.map(|v| VENUES[v].id()),
            "threshold": self.thresholds()[d],
            "orders": self.take_orders(),
            "open_qty": open,
            "pnl": pnl,
            "pnl_bps": pnl / notional * 1e4,
            "outcome": outcome,
        }))?;

        self.check_loss_limit()?;

        self.last_tick_ms = now_ms();
        if self.live.is_some() {
            return self.resync().await;
        }
        self.rebalance()
    }

    pub(super) fn rooms(&self) -> Option<[f64; 2]> {
        let mid = self.mid()?;
        let w = self.free();
        Some([0, 1].map(|d| w[d].quote.min(w[1 - d].base * mid)))
    }

    fn thresholds(&self) -> [f64; 2] {
        let restoring = self.rooms().map_or([false; 2], restoring);
        restoring.map(|r| match r {
            true => self.min_bps.min(self.restore_min_bps),
            false => self.min_bps,
        })
    }

    fn first_leg(&self, b: usize, s: usize) -> usize {
        let ask = self.books[b].borrow().ask_moved;
        let bid = self.books[s].borrow().bid_moved;
        match (ask, bid) {
            (Some(ask), Some(bid)) if ask <= bid => b,
            (Some(_), Some(_)) => s,
            _ if b == BYBIT => b,
            _ => s,
        }
    }

    async fn legs_in_turn(
        &self,
        b: usize,
        s: usize,
        first: usize,
        plan: &Plan,
    ) -> (Result<Fill>, Result<Fill>) {
        let (fee_b, fee_s) = (self.market.fees[b].taker, self.market.fees[s].taker);
        let half_step = self.market.step / 2.0;
        if first == b {
            let buy = self.ioc(b, Side::Buy, plan.qty, plan.buy_limit).await;
            let sell = match &buy {
                Ok(bought) if bought.qty >= half_step => {
                    let floor = break_even_sell(bought.avg_price, fee_b, fee_s);
                    self.ioc(s, Side::Sell, bought.qty, floor).await
                }
                _ => Ok(Fill::default()),
            };
            (buy, sell)
        } else {
            let sell = self.ioc(s, Side::Sell, plan.qty, plan.sell_limit).await;
            let buy = match &sell {
                Ok(sold) if sold.qty >= half_step => {
                    let affordable = self.free()[b].quote / (sold.qty * (1.0 + fee_b));
                    let cap = break_even_buy(sold.avg_price, fee_b, fee_s)
                        .min(plan.buy_limit * (1.0 + SIZE_MARGIN))
                        .min(affordable);
                    self.ioc(b, Side::Buy, sold.qty, cap).await
                }
                _ => Ok(Fill::default()),
            };
            (buy, sell)
        }
    }

    async fn close_remainder(&mut self, excess: f64) -> Result<Option<(usize, Fill, f64)>> {
        let side = if excess > 0.0 { Side::Sell } else { Side::Buy };
        let qty = excess.abs();
        let Some((venue, _, limit)) = (0..2)
            .filter_map(|v| {
                let book = self.books[v].borrow();
                let fee = self.market.fees[v].taker;
                let wallet = &self.inventory.wallets[v];
                match side {
                    Side::Sell => {
                        let (avg, _) = book.sell_fill(qty)?;
                        let limit = book.bids.first()?.0 * (1.0 - REMAINDER_SLIPPAGE);
                        (wallet.base >= qty).then_some((v, avg * (1.0 - fee), limit))
                    }
                    Side::Buy => {
                        let (avg, _) = book.buy_fill(qty)?;
                        let limit = book.asks.first()?.0 * (1.0 + REMAINDER_SLIPPAGE);
                        let cost = qty * limit * (1.0 + fee);
                        (wallet.quote >= cost).then_some((v, -avg * (1.0 + fee), limit))
                    }
                }
            })
            .max_by(|x, y| x.1.total_cmp(&y.1))
        else {
            return Ok(None);
        };
        let fill = self.ioc(venue, side, qty, limit).await?;
        let fee = self.market.fees[venue].taker;
        let cash = self.inventory.wallets[venue].book(side, fill.qty, fill.value(), fee);
        Ok(Some((venue, fill, cash)))
    }
}

pub(super) fn books_fresh(books: [&Book; 2], now: Instant) -> bool {
    books.iter().all(|b| b.fresh_at(now))
}

fn restoring(room: [f64; 2]) -> [bool; 2] {
    [0, 1].map(|d| room[1 - d] < room[d] / 2.0)
}

fn break_even_sell(bought: f64, fee_buy: f64, fee_sell: f64) -> f64 {
    bought * (1.0 + fee_buy) / (1.0 - fee_sell)
}

fn break_even_buy(sold: f64, fee_buy: f64, fee_sell: f64) -> f64 {
    sold * (1.0 - fee_sell) / (1.0 + fee_buy)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_leg_limits_leave_no_loss() {
        let (fee_buy, fee_sell) = (0.0025, 0.001);
        let floor = break_even_sell(1.0, fee_buy, fee_sell);
        assert!((floor * (1.0 - fee_sell) - 1.0 * (1.0 + fee_buy)).abs() < 1e-12);
        let cap = break_even_buy(1.0, fee_buy, fee_sell);
        assert!((1.0 * (1.0 - fee_sell) - cap * (1.0 + fee_buy)).abs() < 1e-12);
        assert!(floor > 1.0 && cap < 1.0);
    }

    #[test]
    fn stale_books_block_signals_until_a_pong() {
        let now = Instant::now();
        let synced = Book {
            synced: true,
            ..Book::default()
        };
        let mut okx = synced.clone();
        let mut bybit = synced.clone();
        assert!(!books_fresh([&okx, &bybit], now));
        okx.pong = Some(now);
        assert!(!books_fresh([&okx, &bybit], now));
        bybit.pong = Some(now);
        assert!(books_fresh([&okx, &bybit], now));
        assert!(!books_fresh(
            [&okx, &bybit],
            now + std::time::Duration::from_secs(5)
        ));
    }

    #[test]
    fn the_direction_with_more_room_restores_balance() {
        assert_eq!(restoring([39.0, 35.0]), [false, false]);
        assert_eq!(restoring([4.0, 70.0]), [false, true]);
        assert_eq!(restoring([70.0, 4.0]), [true, false]);
    }
}
