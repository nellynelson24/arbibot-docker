use anyhow::Result;
use serde_json::json;

use super::market::{Pick, REVIEW_DAYS, load_pair, picks};
use super::orders::floor_to;
use super::{
    Engine, INVENTORY_SLIPPAGE, REMAINDER_SLIPPAGE, VENUES, clock, open_books, wait_for_books,
};
use crate::ledger::now_ms;
use crate::paper::Side;

const SWITCH_STREAK: u32 = 2;
const SWITCH_PAYBACK: f64 = 3.0;
const NEW_HALF_SPREAD: f64 = 0.001;

impl Engine {
    pub(super) async fn review(&mut self) -> Result<bool> {
        if self.live.is_some() || self.halted.is_some() || !self.inventory.transfers.is_empty() {
            return Ok(false);
        }
        let current = format!("{}/{}", self.market.base, self.market.quote);
        println!("[{}] ocena par:", clock());
        let (ranked, best) = picks(
            &self.data_dir,
            self.size,
            self.min_bps,
            self.route_limit,
            &self.http,
            &self.accounts,
        )
        .await?;
        let now_profit = ranked
            .iter()
            .find(|c| c.pair == current)
            .map_or(0.0, |c| c.profit);
        let cost = self.switch_cost();
        let streak = best.as_ref().map_or(0, |b| {
            switch_streak(
                &current,
                now_profit,
                &b.pair,
                b.profit,
                cost,
                self.pending_switch.as_ref(),
            )
        });
        let q = &self.market.quote;
        let verdict = match &best {
            _ if streak >= SWITCH_STREAK => "zmieniam".to_owned(),
            Some(b) if streak > 0 => format!("{} lepsza ({streak}/{SWITCH_STREAK})", b.pair),
            _ => "zostaję".to_owned(),
        };
        let line = format!(
            "obecna {current}: {now_profit:.3} {q}, najlepsza {}: {:.3} {q} (z {REVIEW_DAYS} dni), \
             koszt zmiany ≈{cost:.2} {q} — {verdict}",
            best.as_ref().map_or("-", |b| b.pair.as_str()),
            best.as_ref().map_or(0.0, |b| b.profit),
        );
        println!("  {line}");
        if streak > 0 {
            self.tell(format!("ocena par: {line}"));
        }
        self.ledger.write(json!({
            "type": "review",
            "current": current,
            "current_profit": now_profit,
            "best": best.as_ref().map(|b| &b.pair),
            "best_profit": best.as_ref().map(|b| b.profit),
            "switch_cost": cost,
            "streak": streak,
        }))?;
        self.pending_switch = best
            .as_ref()
            .filter(|_| streak > 0)
            .map(|b| (b.pair.clone(), streak));
        let switched = match best {
            Some(best) if streak >= SWITCH_STREAK => {
                self.pending_switch = None;
                self.switch_to(best).await?
            }
            _ => false,
        };

        self.last_tick_ms = now_ms();
        Ok(switched)
    }

    fn switch_cost(&self) -> f64 {
        let Some(mid) = self.mid() else {
            return f64::INFINITY;
        };
        (0..2)
            .map(|v| {
                let book = self.books[v].borrow();
                let half_spread = match (book.bids.first(), book.asks.first()) {
                    (Some(bid), Some(ask)) => (ask.0 - bid.0) / (ask.0 + bid.0),
                    _ => NEW_HALF_SPREAD,
                };
                let value = self.inventory.wallets[v].base * mid;
                value * (2.0 * self.market.fees[v].taker + half_spread + NEW_HALF_SPREAD)
            })
            .sum()
    }

    async fn switch_to(&mut self, pick: Pick) -> Result<bool> {
        let Some(old_mid) = self.mid() else {
            return Ok(false);
        };
        if pick.quote != self.market.quote {
            println!("  pomijam {}: inna waluta kwotowania", pick.pair);
            return Ok(false);
        }
        let before = self.inventory.equity(old_mid);
        let from = format!("{}/{}", self.market.base, self.market.quote);
        let market = load_pair(
            &self.accounts,
            pick.base,
            pick.quote,
            pick.base_fee,
            pick.quote_fee,
        )
        .await?;
        let (books, trades) = open_books(&market, self.maker.is_some());
        let new_mid = match wait_for_books(&books).await {
            Ok(mid) => mid,
            Err(e) => {
                println!("  pomijam {}: {e:#}", pick.pair);
                return Ok(false);
            }
        };

        for v in 0..2 {
            let w = self.inventory.wallets[v];
            let qty = floor_to(w.base, self.market.step);
            let dust = w.base * old_mid < 0.01 * (w.quote + w.base * old_mid);
            let sellable = qty >= self.market.min_qty
                && qty * old_mid >= self.market.min_notional
                && self.books[v].borrow().sell_fill(qty).is_some();
            let spend = (w.quote + w.base * old_mid) / 2.0;
            let buyable = spend >= market.min_notional.max(1.0)
                && books[v].borrow().buy_fill(spend * 0.99 / new_mid).is_some();
            if !(dust || sellable) || !buyable {
                println!(
                    "  pomijam {}: na {} nie da się sprzedać {:.6} {} albo kupić nowej monety za {spend:.2}",
                    pick.pair,
                    VENUES[v].name(),
                    w.base,
                    self.market.base
                );
                return Ok(false);
            }
        }

        for v in 0..2 {
            let qty = floor_to(self.inventory.wallets[v].base, self.market.step);
            let bid = self.books[v].borrow().bids.first().map(|l| l.0);
            if let Some(bid) = bid
                && qty >= self.market.min_qty
            {
                let fill = self
                    .ioc(v, Side::Sell, qty, bid * (1.0 - REMAINDER_SLIPPAGE))
                    .await?;
                let fee = self.market.fees[v].taker;
                self.inventory.wallets[v].book(Side::Sell, fill.qty, fill.value(), fee);
            }

            self.inventory.wallets[v].base = 0.0;
        }

        self.market = market;
        self.books = books;
        self.trades = trades;
        if let Some(maker) = &mut self.maker {
            maker.reset();
        }
        for v in 0..2 {
            let spend = self.inventory.wallets[v].quote / 2.0;
            let Some(ask) = self.books[v].borrow().asks.first().map(|l| l.0) else {
                continue;
            };
            let m = &self.market;
            let qty = floor_to(spend * 0.99 / ask, m.step);
            if qty < m.min_qty || qty * ask < m.min_notional {
                continue;
            }
            let fill = self
                .ioc(v, Side::Buy, qty, ask * (1.0 + INVENTORY_SLIPPAGE))
                .await?;
            let fee = self.market.fees[v].taker;
            self.inventory.wallets[v].book(Side::Buy, fill.qty, fill.value(), fee);
        }

        let after = self.mid().map_or(before, |mid| self.inventory.equity(mid));
        let cost = before - after;
        self.stats.switches += 1;
        self.stats.switch_cost += cost;
        self.stats.pnl -= cost;
        self.armed = [true; 2];
        let line = format!(
            "zmiana pary {from} → {}: koszt {cost:.4} {}",
            pick.pair, self.market.quote
        );
        println!("[{}] {line}", clock());
        self.tell(line);
        self.ledger.write(json!({
            "type": "switch",
            "from": from,
            "to": pick.pair,
            "cost": cost,
            "wallets": self.inventory.wallets.map(|w| [w.base, w.quote]),
        }))?;
        Ok(true)
    }
}

fn switch_streak(
    current: &str,
    current_profit: f64,
    best: &str,
    best_profit: f64,
    cost: f64,
    pending: Option<&(String, u32)>,
) -> u32 {
    if best == current || best_profit - current_profit <= SWITCH_PAYBACK * cost {
        return 0;
    }
    match pending {
        Some((pair, wins)) if pair == best => wins + 1,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_only_after_a_streak_with_a_margin() {
        let pending = ("STRK/USDC".to_owned(), 1);
        let streak = |current, best, profit, pending| {
            switch_streak(current, 1.0, best, profit, 0.1, pending)
        };
        assert_eq!(streak("STRK/USDC", "STRK/USDC", 1.0, None), 0);
        assert_eq!(streak("CHIP/USDC", "STRK/USDC", 1.2, None), 0);
        assert_eq!(streak("CHIP/USDC", "STRK/USDC", 1.5, None), 1);
        assert_eq!(streak("CHIP/USDC", "STRK/USDC", 1.5, Some(&pending)), 2);
        assert_eq!(streak("CHIP/USDC", "KAIA/USDC", 1.5, Some(&pending)), 1);
    }
}
