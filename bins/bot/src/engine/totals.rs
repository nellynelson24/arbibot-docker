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
            Some("outside_change") => self.outside_change(row["value"].as_f64().unwrap_or(0.0)),
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

    pub(super) fn outside_change(&mut self, change: f64) {
        self.deposits += change;
        if let Some(last) = &mut self.last_equity {
            *last += change;
        }
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

    pub(super) fn summary(
        &self,
        quote: &str,
        equity: Option<f64>,
        today: Option<(f64, u32)>,
    ) -> String {
        self.summary_at(quote, equity, today, now_ms())
    }

    fn summary_at(
        &self,
        quote: &str,
        equity: Option<f64>,
        today: Option<(f64, u32)>,
        now: f64,
    ) -> String {
        let since = |ms: Option<f64>| ms.map_or_else(|| "?".to_owned(), |t| pl_time(t, "%d.%m"));
        let start = since(self.since_ms);
        let mut lines = vec!["Zysk z arbitrażu (po prowizjach i rebalansach):".to_owned()];
        if let Some((pnl, trades)) = today {
            lines.push(format!(
                "• dziś (od {}): {} {quote} z {trades} transakcji",
                pl_time((now / DAY_MS).floor() * DAY_MS, "%H:%M"),
                signed(pnl, 4)
            ));
        }
        lines.push(format!(
            "• 7 dni: {} {quote}",
            signed(self.within(now, 7.0), 4)
        ));
        lines.push(format!(
            "• 30 dni: {} {quote}",
            signed(self.within(now, 30.0), 4)
        ));
        let mut all = format!(
            "• od {start}: {} {quote} z {} transakcji",
            signed(self.pnl, 4),
            self.trades
        );
        let days = self.since_ms.map_or(0.0, |s| (now - s) / DAY_MS);
        if days >= 1.0 {
            let per_day = self.pnl / days;
            all += &format!(", średnio {} {quote}/dzień", signed(per_day, 4));
            if let Some(e) = equity.filter(|e| *e > 0.0) {
                all += &format!(
                    " (~{:.1}% kapitału miesięcznie)",
                    per_day * 30.0 / e * 100.0
                );
            }
        }
        lines.push(all);
        if self.pair_since_ms != self.since_ms {
            lines.push(format!(
                "• na {} od {}: {} {quote} z {} transakcji",
                self.pair,
                since(self.pair_since_ms),
                signed(self.pair_pnl, 4),
                self.pair_trades
            ));
        }
        if let (Some(now_equity), Some(first)) = (equity, self.first_equity) {
            let [first, deposits, pnl, now_equity] =
                [first, self.deposits, self.pnl, now_equity].map(cents);
            lines.extend([
                String::new(),
                format!("Kapitał od {start}:"),
                format!("• na starcie: {first:.2} {quote}"),
                format!(
                    "• zmiany sald poza botem: {} {quote} (wpłaty, wypłaty, zamiany monet)",
                    signed(deposits, 2)
                ),
                format!("• zysk z arbitrażu: {} {quote}", signed(pnl, 2)),
                format!(
                    "• zmiana cen monet: {} {quote}",
                    signed(now_equity - first - deposits - pnl, 2)
                ),
                format!("• teraz: {now_equity:.2} {quote}"),
            ]);
        }
        lines.join("\n")
    }
}

fn cents(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

pub(super) fn signed(x: f64, decimals: usize) -> String {
    let zero = format!("{:.decimals$}", 0.0);
    if format!("{:.decimals$}", x.abs()) == zero {
        return format!("+{zero}");
    }
    format!("{x:+.decimals$}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deposits_while_running_count_once() {
        let t0 = 1_792_000_000_000.0_f64;
        let totals = Totals::from_ledger(vec![
            json!({"type": "start", "ts_ms": t0, "pair": "ARB/USDC", "equity": 100.0}),
            json!({"type": "status", "ts_ms": t0 + 60_000.0, "pnl": 0.0, "equity": 100.0}),
            json!({"type": "outside_change", "ts_ms": t0 + 90_000.0, "value": 50.0}),
            json!({"type": "start", "ts_ms": t0 + 120_000.0, "pair": "ARB/USDC", "equity": 150.0}),
        ]);
        assert!((totals.deposits - 50.0).abs() < 1e-9);
    }

    #[test]
    fn signed_amounts_never_show_negative_zero() {
        assert_eq!(signed(-0.00001, 2), "+0.00");
        assert_eq!(signed(-19.444, 2), "-19.44");
        assert_eq!(signed(4.88113, 4), "+4.8811");
    }

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

        let now = t0 + 10.0 * day;
        let text = totals.summary_at("USDC", Some(150.35), Some((0.55, 2)), now);
        let lines: Vec<&str> = text.lines().collect();
        let midnight = pl_time((now / day).floor() * day, "%H:%M");
        assert_eq!(
            lines,
            [
                "Zysk z arbitrażu (po prowizjach i rebalansach):",
                &format!("• dziś (od {midnight}): +0.5500 USDC z 2 transakcji"),
                "• 7 dni: +0.9500 USDC",
                "• 30 dni: +0.3500 USDC",
                &format!(
                    "• od {}: +0.3500 USDC z 4 transakcji, średnio +0.0350 USDC/dzień (~0.7% kapitału miesięcznie)",
                    pl_time(t0, "%d.%m")
                ),
                &format!(
                    "• na ARB/USDC od {}: +0.9500 USDC z 3 transakcji",
                    pl_time(t0 + 2.0 * day, "%d.%m")
                ),
                "",
                &format!("Kapitał od {}:", pl_time(t0, "%d.%m")),
                "• na starcie: 100.00 USDC",
                "• zmiany sald poza botem: +60.00 USDC (wpłaty, wypłaty, zamiany monet)",
                "• zysk z arbitrażu: +0.35 USDC",
                "• zmiana cen monet: -10.00 USDC",
                "• teraz: 150.35 USDC",
            ]
        );

        let short = totals.summary_at("USDC", None, None, now);
        assert!(!short.contains("Kapitał") && !short.contains("dziś"));
        assert_eq!(short.lines().count(), 5);

        let single = Totals::from_ledger(vec![
            json!({"type": "start", "ts_ms": t0, "pair": "ARB/USDC"}),
            json!({"type": "maker_hedge", "ts_ms": t0, "pnl": 0.1}),
        ]);
        assert!(
            !single
                .summary_at("USDC", None, None, t0)
                .contains("• na ARB/USDC")
        );
    }
}
