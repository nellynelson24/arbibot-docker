use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::json;

use super::{Engine, clock};

const TRADE_REBALANCE_COOLDOWN: Duration = Duration::from_secs(600);
const BLOCKED_FOR: Duration = Duration::from_secs(900);

impl Engine {
    pub(super) async fn rebalance_by_trade(&mut self) -> Result<()> {
        let Some(room) = self.rooms() else {
            return Ok(());
        };
        let min_trade = self.min_trade();
        for (since, &room) in self.blocked_since.iter_mut().zip(&room) {
            if room >= min_trade {
                *since = None;
            } else if since.is_none() {
                *since = Some(Instant::now());
            }
        }
        if self
            .last_trade_rebalance
            .is_some_and(|t| t.elapsed() < TRADE_REBALANCE_COOLDOWN)
        {
            return Ok(());
        }
        let Some((short, amount)) = trade_rebalance_need(room, self.size, min_trade) else {
            return Ok(());
        };
        if self.blocked_since[short].is_none_or(|t| t.elapsed() < BLOCKED_FOR) {
            return Ok(());
        }
        let reverse = 1 - short;
        let Some(plan) = self.plan_sized(reverse, amount) else {
            return Ok(());
        };
        if plan.expected_bps < -self.rebalance_max_cost_bps || !self.has_funds(reverse, &plan) {
            return Ok(());
        }
        self.last_trade_rebalance = Some(Instant::now());
        self.blocked_since[short] = None;
        self.trade(reverse, plan, true).await
    }

    pub(super) fn rebalance(&mut self) -> Result<()> {
        self.inventory.settle();
        if self.trade_rebalance {
            return Ok(());
        }
        let Some(mid) = self.mid() else {
            return Ok(());
        };
        let m = &self.market;
        let Some(r) = self
            .inventory
            .rebalance(mid, m.base_fee, m.quote_fee, self.transfer)
        else {
            return Ok(());
        };
        self.stats.rebalances += 1;
        self.stats.rebalance_cost += r.cost;
        self.stats.pnl -= r.cost;
        self.day_pnl -= r.cost;
        let line = format!(
            "rebalans: {:.4} {} i {:.2} {} w drodze ({} min), koszt {:.4} {}",
            r.base,
            m.base,
            r.quote,
            m.quote,
            self.transfer.as_secs() / 60,
            r.cost,
            m.quote
        );
        println!("[{}] {line}", clock());
        self.tell(line);
        self.ledger.write(json!({
            "type": "rebalance",
            "base": r.base,
            "quote": r.quote,
            "cost": r.cost,
        }))?;
        self.check_loss_limit()
    }
}

fn trade_rebalance_need(room: [f64; 2], size: f64, min_trade: f64) -> Option<(usize, f64)> {
    let short = (0..2).find(|&d| room[d] < min_trade && room[1 - d] >= 3.0 * min_trade)?;
    let amount = ((room[1 - short] - room[short]) / 2.0).min(size);
    (amount >= min_trade).then_some((short, amount))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebalances_only_when_one_side_is_blocked_and_the_other_has_room() {
        assert_eq!(
            trade_rebalance_need([4.0, 70.0], 40.0, 10.0),
            Some((0, 33.0))
        );
        assert_eq!(
            trade_rebalance_need([0.0, 100.0], 40.0, 10.0),
            Some((0, 40.0))
        );
        assert_eq!(trade_rebalance_need([12.0, 70.0], 40.0, 10.0), None);
        assert_eq!(trade_rebalance_need([4.0, 25.0], 40.0, 10.0), None);
    }
}
