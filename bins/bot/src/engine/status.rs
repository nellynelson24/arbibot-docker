use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::json;

use super::rebalance::trade_rebalance_need;
use super::totals::signed;
use super::{BYBIT, Engine, Holding, OKX, SIZE_MARGIN, VENUES, amount, clock};
use crate::inventory::Wallet;
use crate::ledger::now_ms;
use crate::report::pl_time;
use crate::{report, selector};

const STALL_MS: f64 = 3_000.0;
const PAUSE_AFTER_STALL: Duration = Duration::from_secs(30);
const DAY_MS: f64 = 86_400_000.0;
const LIVE_REPORT_GRACE_MS: f64 = 600_000.0;

impl Engine {
    pub(super) fn check_loss_limit(&mut self) -> Result<()> {
        if self.halted.is_none() && self.day_pnl <= -self.daily_loss_limit {
            self.halt(format!(
                "dzienny limit straty ({:.2} {})",
                self.daily_loss_limit, self.market.quote
            ))?;
        }
        Ok(())
    }

    pub(super) fn halt(&mut self, reason: String) -> Result<()> {
        let mut line = format!("ZATRZYMANO: {reason}");
        if self.live.is_some() && !self.stop_file.exists() {
            std::fs::write(&self.stop_file, &reason)
                .with_context(|| format!("zapis {}", self.stop_file.display()))?;
            line += &format!(
                ". Wznowienie: usuń plik {} i uruchom bota ponownie.",
                self.stop_file.display()
            );
        }
        println!("[{}] {line}", clock());
        self.tell(line);
        self.cancel_quotes();
        self.ledger
            .write(json!({"type": "halt", "reason": reason}))?;
        self.halted = Some(reason);
        Ok(())
    }

    pub(super) fn halt_once(&mut self, reason: String) -> Result<()> {
        if self.halted.is_some() {
            println!("[{}] {reason}", clock());
            return Ok(());
        }
        self.halt(reason)
    }

    pub(super) fn restore_day(&mut self) {
        let day = Day::from_rows(&self.ledger.today());
        self.day_pnl += day.pnl;
        self.day_trades += day.trades;
        self.day_failed_hedges += day.failed_hedges;
        self.day_rebalances += day.rebalances;
        if let Some(ts) = day.last_rebalance_ms {
            let ago = Duration::from_secs_f64(((now_ms() - ts) / 1000.0).max(0.0));
            self.last_trade_rebalance = Instant::now().checked_sub(ago);
        }
    }

    pub(super) fn check_stop_file(&mut self) -> Result<()> {
        if self.halted.is_none() && self.stop_file.exists() {
            self.halt(format!("plik {}", self.stop_file.display()))?;
        }
        Ok(())
    }

    pub(super) fn tick(&mut self) -> Result<()> {
        let now = now_ms();
        if stalled(now, self.last_tick_ms) {
            println!(
                "[{}] przerwa w pracy (uśpienie?): pauza do odświeżenia książek",
                clock()
            );
            self.ledger
                .write(json!({"type": "stall", "gap_ms": now - self.last_tick_ms}))?;
            self.tell(format!(
                "przerwa w pracy komputera ({:.0} s): pauza do odświeżenia książek",
                (now - self.last_tick_ms) / 1000.0
            ));
            self.paused_until = Some(Instant::now() + PAUSE_AFTER_STALL);
        }
        self.last_tick_ms = now;
        self.check_stop_file()?;
        let day = selector::utc_date(now);
        if day != self.day {
            let equity = self.mid().map(|mid| self.inventory.equity(mid));
            self.tell(format!(
                "Dzień {} (UTC) zakończony: wynik {} {} z {} transakcji.\n\n{}",
                self.day,
                signed(self.day_pnl, 4),
                self.market.quote,
                self.day_trades,
                self.totals.summary(&self.market.quote, equity, None)
            ));
            self.day = day;
            self.day_pnl = 0.0;
            self.day_trades = 0;
            self.day_rebalances = 0;
            self.day_failed_hedges = 0;
        }
        self.daily_report(now);
        self.inventory.settle();
        self.maker_tick()
    }

    fn daily_report(&mut self, now: f64) {
        let Some(telegram) = &self.telegram else {
            return;
        };
        if self.live.is_none() {
            return;
        }
        let Some(sent) = report::daily_due_after(&self.data_dir, now, LIVE_REPORT_GRACE_MS) else {
            return;
        };
        let day = sent.to_string_lossy().into_owned();
        if self.last_report.as_deref() == Some(day.as_str()) {
            return;
        }
        self.last_report = Some(day);
        let (data_dir, tx) = (self.data_dir.clone(), telegram.sender());
        tokio::task::spawn_blocking(move || {
            let text = match report::build(&data_dir, now - DAY_MS, now, None) {
                Ok(r) => {
                    let _ = report::save(&data_dir, &r, now);
                    let _ = std::fs::write(&sent, "");
                    r.short
                }
                Err(e) => format!("raport dzienny nieudany: {e:#}"),
            };
            let _ = tx.send(text);
        });
    }

    pub(super) fn status(&mut self) -> Result<()> {
        let Some(mid) = self.mid() else {
            return Ok(());
        };
        let equity = self.inventory.equity(mid);
        let mut blocked = Vec::new();
        let mut key = String::new();
        if self.live.is_some() {
            let w = self.inventory.wallets;
            let min_trade = self.min_trade();
            let need = self
                .rooms()
                .and_then(|room| trade_rebalance_need(room, self.size, min_trade));
            let cost = need
                .and_then(|(short, amount)| self.plan_sized(1 - short, amount))
                .map(|plan| -plan.expected_bps);
            for d in 0..2 {
                if w[d].quote < min_trade * (1.0 + SIZE_MARGIN) || w[1 - d].base * mid < min_trade {
                    let can = need.is_some_and(|(short, _)| short == d);
                    blocked.push(format!(
                        "brak środków na kup {} → sprzedaj {} ({})",
                        VENUES[d].name(),
                        VENUES[1 - d].name(),
                        blocked_hint(
                            self.trade_rebalance,
                            self.lazy_rebalance,
                            can.then_some(cost).flatten(),
                            can,
                            self.rebalance_max_cost_bps
                        )
                    ));
                    key += &format!("{d}{can} ");
                }
            }
        }
        if key != self.last_blocked {
            if !blocked.is_empty() {
                self.tell(blocked.join("\n"));
            }
            self.last_blocked = key;
        }
        let s = &self.stats;
        let gaps = [0, 1].map(|v| self.books[v].borrow().gaps);
        let mut extra = String::new();
        if self.trade_rebalance {
            extra += &format!(
                " | rebalans handlem {} ({:+.4})",
                s.rebalance_trades, s.rebalance_trade_pnl
            );
        }
        if self.maker.is_some() {
            extra += &format!(
                " | maker: wypełnienia {}, zabezpieczenia {} (nietrafione {}), wynik {:+.4}",
                s.maker_fills, s.maker_hedges, s.hedge_misses, s.maker_pnl
            );
        }
        if gaps.iter().any(|&g| g > 0) {
            extra += &format!(" | luki książek OKX {} / Bybit {}", gaps[0], gaps[1]);
        }
        if let Some(reason) = &self.halted {
            extra += &format!(" | ZATRZYMANY: {reason}");
        }
        for b in &blocked {
            extra += &format!(" | {b}");
        }
        println!(
            "[{}] sygnały {} (bez środków {}), transakcje taker {} (sparowane {}, z resztą {}, \
             puste {}), zysk {:+.4} {q}, kapitał {:.2} {q} ({:+.2} z cen), rebalanse {}{extra}",
            clock(),
            s.signals,
            s.no_funds,
            s.trades,
            s.matched,
            s.with_remainder,
            s.unfilled,
            s.pnl,
            equity,
            equity - self.start_equity - s.pnl - s.outside_change,
            s.rebalances,
            q = self.market.quote
        );
        if let Some(telegram) = &self.telegram {
            telegram.set_status(self.status_text(mid, equity, &blocked, gaps));
        }
        self.ledger.write(json!({
            "type": "status",
            "signals": s.signals,
            "no_funds": s.no_funds,
            "trades": s.trades,
            "matched": s.matched,
            "with_remainder": s.with_remainder,
            "unfilled": s.unfilled,
            "pnl": s.pnl,
            "equity": equity,
            "mid": mid,
            "wallets": self.inventory.wallets.map(|w| [w.base, w.quote]),
            "rebalances": s.rebalances,
            "rebalance_cost": s.rebalance_cost,
            "switches": s.switches,
            "switch_cost": s.switch_cost,
            "rebalance_trades": s.rebalance_trades,
            "rebalance_trade_pnl": s.rebalance_trade_pnl,
            "maker_fills": s.maker_fills,
            "maker_hedges": s.maker_hedges,
            "maker_pnl": s.maker_pnl,
            "hedge_misses": s.hedge_misses,
            "outside_change": s.outside_change,
            "halted": self.halted,
            "book_gaps": gaps,
        }))
    }

    fn status_text(&self, mid: f64, equity: f64, blocked: &[String], gaps: [u64; 2]) -> String {
        let (base, q) = (&self.market.base, &self.market.quote);
        let s = &self.stats;
        let now = now_ms();
        let mut lines = vec![format!(
            "Stan {} · {base}/{q} · {}",
            pl_time(now, "%d.%m %H:%M"),
            if self.live.is_some() {
                "NA ŻYWO"
            } else {
                "na sucho"
            }
        )];
        let mut notes: Vec<String> = self
            .halted
            .iter()
            .map(|reason| format!("ZATRZYMANY: {reason}"))
            .collect();
        notes.extend(blocked.iter().cloned());
        if !notes.is_empty() {
            lines.push(String::new());
            lines.push("Uwagi:".to_owned());
            lines.extend(notes.iter().map(|n| format!("• {n}")));
        }
        lines.push(String::new());
        lines.extend(self.capital_lines(equity, mid));

        let elapsed = self.started.elapsed();
        lines.push(String::new());
        lines.push(format!(
            "Od restartu ({}, {}):",
            pl_time(now - elapsed.as_secs_f64() * 1e3, "%d.%m %H:%M"),
            duration(elapsed.as_secs())
        ));
        lines.push(format!("• zysk z arbitrażu: {} {q}", signed(s.pnl, 4)));
        if self.maker.is_some() {
            lines.push(format!(
                "• maker: wypełnienia {} na {}, zabezpieczenia {} na {} (nietrafione {}), wynik {} {q}",
                s.maker_fills,
                VENUES[OKX].name(),
                s.maker_hedges,
                VENUES[BYBIT].name(),
                s.hedge_misses,
                signed(s.maker_pnl, 4)
            ));
        }
        lines.push(format!(
            "• taker: wykonane {} z {} prób (sparowane {}, z resztą {}, puste {}), sygnały {} (bez środków {})",
            s.matched + s.with_remainder,
            s.trades,
            s.matched,
            s.with_remainder,
            s.unfilled,
            s.signals,
            s.no_funds
        ));
        if self.trade_rebalance {
            lines.push(format!(
                "• rebalans handlem: {} ({} {q})",
                s.rebalance_trades,
                signed(s.rebalance_trade_pnl, 4)
            ));
        }
        if s.rebalances > 0 {
            lines.push(format!(
                "• rebalans przelewem: {} (koszt {:.4} {q})",
                s.rebalances, s.rebalance_cost
            ));
        }
        if s.outside_change != 0.0 {
            lines.push(format!(
                "• zmiany sald poza botem: {} {q}",
                signed(s.outside_change, 2)
            ));
        }
        lines.push(format!(
            "• zmiana cen {base}: {} {q}",
            signed(equity - self.start_equity - s.pnl - s.outside_change, 2)
        ));
        if gaps.iter().any(|&g| g > 0) {
            lines.push(format!(
                "• przerwy w danych książek: {} {}, {} {}",
                VENUES[OKX].name(),
                gaps[0],
                VENUES[BYBIT].name(),
                gaps[1]
            ));
        }
        lines.extend(self.outside_section(mid));
        lines.push(String::new());
        lines.push(
            self.totals
                .summary(q, Some(equity), Some((self.day_pnl, self.day_trades))),
        );
        lines.join("\n")
    }

    pub(super) fn capital_lines(&self, equity: f64, mid: f64) -> Vec<String> {
        let (base, q) = (&self.market.base, &self.market.quote);
        let mut lines = vec![format!(
            "Kapitał bota: {equity:.2} {q} ({base} i {q} na kontach handlowych, {base} po {mid:.6})"
        )];
        lines.extend(
            balances(&self.inventory.wallets, base, q)
                .into_iter()
                .map(|b| format!("• {b}")),
        );
        lines
    }

    pub(super) fn outside_section(&self, mid: f64) -> Vec<String> {
        let held = outside_lines(&self.outside, &self.market.base, &self.market.quote, mid);
        if held.is_empty() {
            return held;
        }
        let mut lines = vec![
            String::new(),
            "Poza botem (nie wchodzi do kapitału bota):".to_owned(),
        ];
        lines.extend(held.into_iter().map(|h| format!("• {h}")));
        lines
    }

    pub(super) fn summary(&mut self) -> Result<()> {
        self.status()?;
        let elapsed = self.started.elapsed();
        let hours = elapsed.as_secs_f64() / 3600.0;
        let s = &self.stats;
        let q = &self.market.quote;
        let mut lines = vec![format!("Koniec pracy po {}:", duration(elapsed.as_secs()))];
        let mut profit = format!("• zysk z arbitrażu: {} {q}", signed(s.pnl, 4));
        if hours >= 1.0 {
            profit += &format!(", na godzinę {} {q}", signed(s.pnl / hours, 4));
        }
        lines.push(profit);
        if self.maker.is_some() {
            lines.push(format!(
                "• maker: zabezpieczenia {}, wynik {} {q}",
                s.maker_hedges,
                signed(s.maker_pnl, 4)
            ));
        }
        lines.push(format!(
            "• taker: wykonane {} z {} prób",
            s.matched + s.with_remainder,
            s.trades
        ));
        if s.rebalances > 0 {
            lines.push(format!(
                "• rebalans przelewem: {} (koszt {:.4} {q})",
                s.rebalances, s.rebalance_cost
            ));
        }
        if s.switches > 0 {
            lines.push(format!(
                "• zmiany pary: {} (koszt {:.4} {q})",
                s.switches, s.switch_cost
            ));
        }
        let text = lines.join("\n");
        println!("\n{text}");
        self.tell(text);
        Ok(())
    }
}

fn stalled(now: f64, last_tick_ms: f64) -> bool {
    now - last_tick_ms > STALL_MS
}

#[derive(Default)]
struct Day {
    pnl: f64,
    trades: u32,
    failed_hedges: u32,
    rebalances: u32,
    last_rebalance_ms: Option<f64>,
}

impl Day {
    fn from_rows(rows: &[serde_json::Value]) -> Self {
        let mut day = Day::default();
        for row in rows {
            let pnl = row["pnl"].as_f64().unwrap_or(0.0);
            match row["type"].as_str() {
                Some("trade") => {
                    day.pnl += pnl;
                    day.trades += u32::from(row["outcome"] != "brak wypełnienia");
                }
                Some("maker_hedge") => {
                    day.pnl += pnl;
                    day.trades += 1;
                    day.failed_hedges += u32::from(!row["flatten"].is_null());
                }
                Some("rebalance_trade") => {
                    day.pnl += pnl;
                    day.rebalances += 1;
                    day.last_rebalance_ms = row["ts_ms"].as_f64().or(day.last_rebalance_ms);
                }
                Some("rebalance") => day.pnl -= row["cost"].as_f64().unwrap_or(0.0),
                _ => {}
            }
        }
        day
    }
}

fn blocked_hint(
    trade_rebalance: bool,
    lazy: bool,
    cost: Option<f64>,
    can_rebalance: bool,
    limit: f64,
) -> String {
    if !(trade_rebalance && can_rebalance) {
        return "za mało środków na rebalans handlem — przelej ręcznie".to_owned();
    }
    match cost {
        Some(c) if c > limit => format!(
            "rebalans handlem teraz za drogi: {c:.0} bp przy limicie {limit:.0} — salda wyrówna maker albo tańszy moment"
        ),
        _ => format!(
            "bot wyrówna salda handlem {}{}",
            if lazy {
                "po straconym sygnale"
            } else {
                "po 15 min"
            },
            cost.map_or_else(String::new, |c| format!(" (koszt teraz {c:.0} bp)"))
        ),
    }
}

fn balances(wallets: &[Wallet; 2], base: &str, quote: &str) -> Vec<String> {
    VENUES
        .iter()
        .zip(wallets)
        .map(|(v, w)| {
            format!(
                "{}: {} {base} i {:.2} {quote}",
                v.name(),
                amount(w.base, 4),
                w.quote
            )
        })
        .collect()
}

fn outside_lines(held: &[Vec<Holding>; 2], base: &str, quote: &str, mid: f64) -> Vec<String> {
    let mut rows: Vec<(Option<f64>, String)> = Vec::new();
    for (venue, list) in VENUES.iter().zip(held) {
        for h in list {
            let (value, unit) = if h.asset == base {
                (Some(h.qty * mid), quote)
            } else if h.asset == quote {
                (Some(h.qty), quote)
            } else {
                (h.usd, "USD")
            };
            if value.is_some_and(|v| v < 1.0) {
                continue;
            }
            let account = match h.account {
                "funding" => "konto funding",
                _ => "konto handlowe",
            };
            let worth = match value {
                Some(v) if h.asset != quote => format!(" ≈ {v:.2} {unit}"),
                Some(_) => String::new(),
                None => " (bez wyceny)".to_owned(),
            };
            let qty = if h.asset == quote {
                format!("{:.2}", h.qty)
            } else {
                amount(h.qty, 4)
            };
            rows.push((
                value,
                format!("{}, {account}: {qty} {}{worth}", venue.name(), h.asset),
            ));
        }
    }
    rows.sort_by(|a, b| b.0.unwrap_or(-1.0).total_cmp(&a.0.unwrap_or(-1.0)));
    rows.into_iter().map(|(_, line)| line).collect()
}

fn duration(secs: u64) -> String {
    let (days, hours, mins) = (secs / 86_400, secs / 3600 % 24, secs / 60 % 60);
    match (days, hours) {
        (0, 0) => format!("{mins} min"),
        (0, _) => format!("{hours} h {mins} min"),
        _ => format!("{days} d {hours} h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_hint_says_whether_and_when_trading_rebalances() {
        assert_eq!(
            blocked_hint(true, false, None, true, 45.0),
            "bot wyrówna salda handlem po 15 min"
        );
        assert_eq!(
            blocked_hint(true, true, Some(38.4), true, 45.0),
            "bot wyrówna salda handlem po straconym sygnale (koszt teraz 38 bp)"
        );
        assert!(
            blocked_hint(true, true, Some(84.6), true, 30.0)
                .starts_with("rebalans handlem teraz za drogi: 85 bp przy limicie 30")
        );
        assert!(blocked_hint(true, false, None, false, 45.0).contains("przelej ręcznie"));
        assert!(blocked_hint(false, false, Some(10.0), true, 45.0).contains("przelej ręcznie"));
    }

    #[test]
    fn status_lists_balances_on_both_venues() {
        let wallets = [
            Wallet {
                base: 592.196,
                quote: 22.3612,
            },
            Wallet {
                base: -0.00001,
                quote: 80.93,
            },
        ];
        assert_eq!(
            balances(&wallets, "ARB", "USDC"),
            [
                format!("{}: 592.1960 ARB i 22.36 USDC", VENUES[0].name()),
                format!("{}: 0.0000 ARB i 80.93 USDC", VENUES[1].name()),
            ]
        );
    }

    #[test]
    fn outside_holdings_are_valued_and_dust_is_hidden() {
        let hold = |account, asset: &str, qty, usd| Holding {
            account,
            asset: asset.to_owned(),
            qty,
            usd,
        };
        let held = [
            vec![
                hold("trading", "STRK", 190.12, Some(32.104)),
                hold("trading", "PEPE", 1000.0, Some(0.01)),
                hold("funding", "USDC", 5.0, None),
            ],
            vec![
                hold("funding", "ARB", 10.0, None),
                hold("funding", "XYZ", 3.0, None),
            ],
        ];
        assert_eq!(
            outside_lines(&held, "ARB", "USDC", 0.18),
            [
                format!(
                    "{}, konto handlowe: 190.1200 STRK ≈ 32.10 USD",
                    VENUES[0].name()
                ),
                format!("{}, konto funding: 5.00 USDC", VENUES[0].name()),
                format!(
                    "{}, konto funding: 10.0000 ARB ≈ 1.80 USDC",
                    VENUES[1].name()
                ),
                format!(
                    "{}, konto funding: 3.0000 XYZ (bez wyceny)",
                    VENUES[1].name()
                ),
            ]
        );
    }

    #[test]
    fn a_slow_start_is_not_a_stall() {
        let started = 1_794_000_000_000.0;
        assert!(!stalled(started + 1_000.0, started));
        assert!(stalled(started + 5_000.0, started));
    }

    #[test]
    fn day_counters_survive_a_restart() {
        let rows = [
            json!({"type": "maker_hedge", "pnl": 0.05, "ts_ms": 1.0}),
            json!({"type": "maker_hedge", "pnl": -0.02, "flatten": "170131", "ts_ms": 2.0}),
            json!({"type": "maker_hedge", "pnl": -0.01, "flatten": "170131", "ts_ms": 3.0}),
            json!({"type": "maker_hedge", "pnl": -0.01, "flatten": "170131", "ts_ms": 4.0}),
            json!({"type": "trade", "pnl": 0.0, "outcome": "brak wypełnienia", "ts_ms": 5.0}),
            json!({"type": "trade", "pnl": 0.03, "outcome": "sparowana", "ts_ms": 6.0}),
            json!({"type": "rebalance_trade", "pnl": -0.08, "outcome": "sparowana", "ts_ms": 7.0}),
        ];
        let day = Day::from_rows(&rows);
        assert_eq!((day.trades, day.failed_hedges, day.rebalances), (5, 3, 1));
        assert_eq!(day.last_rebalance_ms, Some(7.0));
        assert!((day.pnl + 0.04).abs() < 1e-9);
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration(59), "0 min");
        assert_eq!(duration(2 * 3600 + 18 * 60), "2 h 18 min");
        assert_eq!(duration(3 * 86_400 + 4 * 3600 + 59), "3 d 4 h");
    }
}
