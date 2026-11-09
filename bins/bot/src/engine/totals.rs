use serde_json::Value;

use crate::ledger::now_ms;
use crate::report::pl_time;

const DAY_MS: f64 = 86_400_000.0;

#[derive(Default)]
pub(super) struct Totals {
    pnl: f64,
    trades: u32,
    since_ms: Option<f64>,
    pair: String,
    pair_pnl: f64,
    pair_trades: u32,
    pair_since_ms: Option<f64>,
    events: Vec<(f64, f64)>,
    first_equity: Option<f64>,
    last_equity: Option<f64>,
    deposits: f64,
    awaiting_equity: bool,
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
        let ts = row["ts_ms"].as_f64().unwrap_or(0.0);
        match row["type"].as_str() {
            Some("start") => {
                let pair = row["pair"].as_str().unwrap_or_default();
                if pair != self.pair {
                    self.pair = pair.to_owned();
                    self.pair_pnl = 0.0;
                    self.pair_trades = 0;
                    self.pair_since_ms = Some(ts);
                }
                self.since_ms = self.since_ms.or(Some(ts));
                match row["equity"].as_f64() {
                    Some(equity) => self.started_with(equity),
                    None => self.awaiting_equity = true,
                }
            }
            Some("status") => {
                if let Some(equity) = row["equity"].as_f64() {
                    if self.awaiting_equity {
                        self.started_with(equity - pnl);
                    }
                    self.last_equity = Some(equity);
                }
            }
            Some("trade" | "maker_hedge") => self.add_at(ts, pnl, true),
            Some("rebalance_trade") => self.add_at(ts, pnl, false),
            Some("rebalance") => self.add_at(ts, -row["cost"].as_f64().unwrap_or(0.0), false),
            _ => {}
        }
    }

    fn started_with(&mut self, equity: f64) {
        self.awaiting_equity = false;
        match (self.first_equity, self.last_equity) {
            (None, _) => self.first_equity = Some(equity),
            (Some(_), Some(last)) => self.deposits += equity - last,
            (Some(_), None) => {}
        }
        self.last_equity = Some(equity);
    }

    pub(super) fn add(&mut self, pnl: f64, counted: bool) {
        self.add_at(now_ms(), pnl, counted);
    }

    fn add_at(&mut self, ts: f64, pnl: f64, counted: bool) {
        self.pnl += pnl;
        self.pair_pnl += pnl;
        self.events.push((ts, pnl));
        if counted {
            self.trades += 1;
            self.pair_trades += 1;
        }
    }

    fn within(&self, now: f64, days: f64) -> f64 {
        self.events
            .iter()
            .filter(|(t, _)| *t >= now - days * DAY_MS)
            .map(|(_, p)| p)
            .sum()
    }

    pub(super) fn summary(&self, quote: &str, equity: Option<f64>) -> String {
        self.summary_at(quote, equity, now_ms())
    }

    fn summary_at(&self, quote: &str, equity: Option<f64>, now: f64) -> String {
        let since = |ms: Option<f64>| ms.map_or_else(|| "?".to_owned(), |t| pl_time(t, "%d.%m"));
        let start = since(self.since_ms);
        let profit = format!(
            "zysk z arbitrażu {:+.4} {quote} od {start} (z {} transakcji)",
            self.pnl, self.trades
        );
        let mut text = match equity {
            Some(e) => format!("Obecny kapitał: {e:.2} {quote}, w tym {profit}"),
            None => format!("Razem {profit}"),
        };
        if self.pair_since_ms != self.since_ms {
            text += &format!(
                "; na {} od {}: {:+.4} {quote} z {} transakcji",
                self.pair,
                since(self.pair_since_ms),
                self.pair_pnl,
                self.pair_trades
            );
        }
        text += ".";
        if let (Some(e), Some(first)) = (equity, self.first_equity) {
            text += &format!(
                "\nOd {start}: kapitał początkowy {first:.2}, zmiany sald poza botem {:+.2} (wpłaty, wypłaty, zamiany monet), zmiana cen monet {:+.2} {quote}.",
                self.deposits,
                e - first - self.deposits - self.pnl
            );
        }
        let days = self.since_ms.map_or(0.0, |s| (now - s) / DAY_MS);
        text += &format!(
            "\nZysk z arbitrażu: 7 dni {:+.4}, 30 dni {:+.4} {quote}",
            self.within(now, 7.0),
            self.within(now, 30.0)
        );
        if days >= 1.0 {
            let per_day = self.pnl / days;
            text += &format!(", średnio {per_day:+.4} {quote}/dzień");
            if let Some(e) = equity.filter(|e| *e > 0.0) {
                text += &format!(
                    " (~{:.1}% kapitału miesięcznie)",
                    per_day * 30.0 / e * 100.0
                );
            }
        }
        text + "."
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn totals_survive_restarts_and_restart_per_pair() {
        let day = 86_400_000.0;
        let t0 = 1_792_000_000_000.0_f64;
        let rows = vec![
            json!({"type": "start", "ts_ms": t0, "pair": "TIA/USDC"}),
            json!({"type": "status", "ts_ms": t0 + 60_000.0, "pnl": 0.0, "equity": 100.0}),
            json!({"type": "maker_hedge", "ts_ms": t0 + day, "pnl": -0.5}),
            json!({"type": "rebalance_trade", "ts_ms": t0 + day, "pnl": -0.1}),
            json!({"type": "status", "ts_ms": t0 + day, "pnl": -0.6, "equity": 98.0}),
            json!({"type": "start", "ts_ms": t0 + 2.0 * day, "pair": "ARB/USDC"}),
            json!({"type": "status", "ts_ms": t0 + 2.0 * day, "pnl": 0.0, "equity": 158.0}),
            json!({"type": "maker_hedge", "ts_ms": t0 + 9.0 * day, "pnl": 0.4}),
            json!({"type": "status", "ts_ms": t0 + 9.0 * day, "pnl": 99.0, "equity": 157.0}),
            json!({"type": "start", "ts_ms": t0 + 10.0 * day, "pair": "ARB/USDC", "equity": 157.0}),
            json!({"type": "trade", "ts_ms": t0 + 10.0 * day, "pnl": 0.3}),
        ];
        let mut totals = Totals::from_ledger(rows);
        totals.add_at(t0 + 10.0 * day, 0.25, true);
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        assert!(close(totals.pnl, 0.35) && totals.trades == 4);
        assert!(close(totals.pair_pnl, 0.95) && totals.pair_trades == 3);
        assert!(close(totals.deposits, 60.0));

        let text = totals.summary_at("USDC", Some(150.35), t0 + 10.0 * day);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(
            lines[0].starts_with(
                "Obecny kapitał: 150.35 USDC, w tym zysk z arbitrażu +0.3500 USDC od "
            )
        );
        assert!(lines[0].contains(" (z 4 transakcji); na ARB/USDC od "));
        assert!(lines[0].ends_with("+0.9500 USDC z 3 transakcji."));
        assert!(lines[1].ends_with(
            "kapitał początkowy 100.00, zmiany sald poza botem +60.00 (wpłaty, wypłaty, zamiany monet), zmiana cen monet -10.00 USDC."
        ));
        assert_eq!(
            lines[2],
            "Zysk z arbitrażu: 7 dni +0.9500, 30 dni +0.3500 USDC, średnio +0.0350 USDC/dzień (~0.7% kapitału miesięcznie)."
        );

        let short = totals.summary_at("USDC", None, t0 + 10.0 * day);
        assert!(short.starts_with("Razem zysk z arbitrażu +0.3500 USDC od "));
        assert_eq!(short.lines().count(), 2);

        let single = Totals::from_ledger(vec![
            json!({"type": "start", "ts_ms": t0, "pair": "ARB/USDC"}),
            json!({"type": "maker_hedge", "ts_ms": t0, "pnl": 0.1}),
        ]);
        assert!(!single.summary_at("USDC", None, t0).contains("na ARB/USDC"));
    }
}
