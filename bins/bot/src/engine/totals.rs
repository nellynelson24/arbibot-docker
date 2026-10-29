use serde_json::Value;

use crate::report::pl_time;

#[derive(Default)]
pub(super) struct Totals {
    pnl: f64,
    trades: u32,
    since_ms: Option<f64>,
    pair: String,
    pair_pnl: f64,
    pair_trades: u32,
    pair_since_ms: Option<f64>,
}

impl Totals {
    pub(super) fn from_ledger(rows: impl IntoIterator<Item = Value>) -> Self {
        let mut totals = Totals::default();
        for row in rows {
            totals.read(&row);
        }
        totals
    }

    fn read(&mut self, row: &Value) {
        let pnl = row["pnl"].as_f64().unwrap_or(0.0);
        match row["type"].as_str() {
            Some("start") => {
                let ts = row["ts_ms"].as_f64();
                let pair = row["pair"].as_str().unwrap_or_default();
                if pair != self.pair {
                    self.pair = pair.to_owned();
                    self.pair_pnl = 0.0;
                    self.pair_trades = 0;
                    self.pair_since_ms = ts;
                }
                self.since_ms = self.since_ms.or(ts);
            }
            Some("trade" | "maker_hedge") => self.add(pnl, true),
            Some("rebalance_trade") => self.add(pnl, false),
            Some("rebalance") => self.add(-row["cost"].as_f64().unwrap_or(0.0), false),
            _ => {}
        }
    }

    pub(super) fn add(&mut self, pnl: f64, counted: bool) {
        self.pnl += pnl;
        self.pair_pnl += pnl;
        if counted {
            self.trades += 1;
            self.pair_trades += 1;
        }
    }

    pub(super) fn summary(&self, quote: &str) -> String {
        let since = |ms: Option<f64>| ms.map_or_else(|| "?".to_owned(), |t| pl_time(t, "%d.%m"));
        let all = format!(
            "Razem od początku ({}): {:+.4} {quote} z {} transakcji",
            since(self.since_ms),
            self.pnl,
            self.trades
        );
        if self.pair_since_ms == self.since_ms {
            return format!("{all}.");
        }
        format!(
            "{all}; na {} od {}: {:+.4} {quote} z {} transakcji.",
            self.pair,
            since(self.pair_since_ms),
            self.pair_pnl,
            self.pair_trades
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn totals_survive_restarts_and_restart_per_pair() {
        let day = 86_400_000.0;
        let rows = vec![
            json!({"type": "start", "ts_ms": 1_792_000_000_000.0_f64, "pair": "TIA/USDC"}),
            json!({"type": "maker_hedge", "pnl": -0.5}),
            json!({"type": "rebalance_trade", "pnl": -0.1}),
            json!({"type": "start", "ts_ms": 1_792_000_000_000.0_f64 + 2.0 * day, "pair": "ARB/USDC"}),
            json!({"type": "maker_hedge", "pnl": 0.4}),
            json!({"type": "status", "pnl": 99.0}),
            json!({"type": "start", "ts_ms": 1_792_000_000_000.0_f64 + 3.0 * day, "pair": "ARB/USDC"}),
            json!({"type": "trade", "pnl": 0.3}),
        ];
        let mut totals = Totals::from_ledger(rows);
        totals.add(0.25, true);
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        assert!(close(totals.pnl, 0.35) && totals.trades == 4);
        assert!(close(totals.pair_pnl, 0.95) && totals.pair_trades == 3);
        let text = totals.summary("USDC");
        assert!(text.starts_with("Razem od początku ("));
        assert!(text.contains("+0.3500 USDC z 4 transakcji; na ARB/USDC od "));
        assert!(text.ends_with("+0.9500 USDC z 3 transakcji."));

        let single = Totals::from_ledger(vec![
            json!({"type": "start", "ts_ms": 1_792_000_000_000.0_f64, "pair": "ARB/USDC"}),
            json!({"type": "maker_hedge", "pnl": 0.1}),
        ]);
        assert!(!single.summary("USDC").contains("na ARB/USDC"));
    }
}
