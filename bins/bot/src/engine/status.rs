use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::json;

use super::rebalance::trade_rebalance_need;
use super::{Engine, SIZE_MARGIN, VENUES, clock};
use crate::ledger::now_ms;
use crate::{report, selector};

const STALL_MS: f64 = 3_000.0;
const PAUSE_AFTER_STALL: Duration = Duration::from_secs(30);
const DAY_MS: f64 = 86_400_000.0;

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

    pub(super) fn restore_day(&mut self) {
        for entry in self.ledger.today() {
            let pnl = entry["pnl"].as_f64().unwrap_or(0.0);
            match entry["type"].as_str() {
                Some("trade" | "maker_hedge") => {
                    self.day_pnl += pnl;
                    self.day_trades += 1;
                }
                Some("rebalance_trade") => self.day_pnl += pnl,
                Some("rebalance") => self.day_pnl -= entry["cost"].as_f64().unwrap_or(0.0),
                _ => {}
            }
        }
    }

    pub(super) fn tick(&mut self) -> Result<()> {
        let now = now_ms();
        if now - self.last_tick_ms > STALL_MS {
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
        if self.halted.is_none() && self.stop_file.exists() {
            self.halt(format!("plik {}", self.stop_file.display()))?;
        }
        let day = selector::utc_date(now);
        if day != self.day {
            self.tell(format!(
                "dzień {} zakończony: transakcje {}, wynik {:+.4} {}",
                self.day, self.day_trades, self.day_pnl, self.market.quote
            ));
            self.day = day;
            self.day_pnl = 0.0;
            self.day_trades = 0;
            self.day_rebalances = 0;
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
        let Some(sent) = report::daily_due(&self.data_dir, now) else {
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
        let mut blocked = String::new();
        if self.live.is_some() {
            let w = &self.inventory.wallets;
            let min_trade = self.min_trade();
            let short = self
                .rooms()
                .and_then(|room| trade_rebalance_need(room, self.size, min_trade))
                .map(|(short, _)| short);
            for d in 0..2 {
                if w[d].quote < min_trade * (1.0 + SIZE_MARGIN) || w[1 - d].base * mid < min_trade {
                    blocked += &format!(
                        " | brak środków na kup {} → sprzedaj {} ({})",
                        VENUES[d].name(),
                        VENUES[1 - d].name(),
                        blocked_hint(self.trade_rebalance, self.lazy_rebalance, short == Some(d))
                    );
                }
            }
        }
        if blocked != self.last_blocked {
            if !blocked.is_empty() {
                self.tell(blocked.trim_start_matches(" | ").replace(" | ", "\n"));
            }
            self.last_blocked = blocked.clone();
        }
        let s = &self.stats;
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
        let gaps = [0, 1].map(|v| self.books[v].borrow().gaps);
        if gaps.iter().any(|&g| g > 0) {
            extra += &format!(" | luki książek OKX {} / Bybit {}", gaps[0], gaps[1]);
        }
        if let Some(reason) = &self.halted {
            extra += &format!(" | ZATRZYMANY: {reason}");
        }
        let line = format!(
            "sygnały {} (bez środków {}), transakcje {} (sparowane {}, z resztą {}, puste {}), \
             zysk {:+.4} {q}, kapitał {:.2} {q} ({:+.2} z cen), rebalanse {}{extra}{blocked}",
            s.signals,
            s.no_funds,
            s.trades,
            s.matched,
            s.with_remainder,
            s.unfilled,
            s.pnl,
            equity,
            equity - self.start_equity - s.pnl,
            s.rebalances,
            q = self.market.quote
        );
        println!("[{}] {line}", clock());
        if let Some(telegram) = &self.telegram {
            telegram.set_status(format!(
                "{} {}/{}: {line}",
                clock(),
                self.market.base,
                self.market.quote
            ));
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
            "halted": self.halted,
            "book_gaps": gaps,
        }))
    }

    pub(super) fn summary(&mut self) -> Result<()> {
        self.status()?;
        let hours = self.started.elapsed().as_secs_f64() / 3600.0;
        let s = &self.stats;
        let q = &self.market.quote;

        let per_hour = if hours >= 1.0 {
            format!("; na godzinę {:+.4} {q}", s.pnl / hours)
        } else {
            String::new()
        };
        let line = format!(
            "Podsumowanie ({hours:.1} h): transakcje {}, zysk z arbitrażu {:+.4} {q} \
             (w tym maker {:+.4} z {} zabezpieczeń, koszt rebalansu {:.4}, zmian pary {} za {:.4}){per_hour}.",
            s.trades,
            s.pnl,
            s.maker_pnl,
            s.maker_hedges,
            s.rebalance_cost,
            s.switches,
            s.switch_cost,
        );
        println!("\n{line}");
        self.tell(format!("koniec pracy. {line}"));
        Ok(())
    }
}

fn blocked_hint(trade_rebalance: bool, lazy: bool, can_rebalance: bool) -> &'static str {
    match (trade_rebalance && can_rebalance, lazy) {
        (true, false) => "bot wyrówna salda handlem po 15 min",
        (true, true) => "bot wyrówna salda handlem po straconym sygnale",
        (false, _) => "za mało środków na rebalans handlem — przelej ręcznie",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_hint_asks_for_a_transfer_only_when_trading_cannot_rebalance() {
        assert_eq!(
            blocked_hint(true, false, true),
            "bot wyrówna salda handlem po 15 min"
        );
        assert!(blocked_hint(true, true, true).contains("straconym sygnale"));
        assert!(blocked_hint(true, false, false).contains("przelej ręcznie"));
        assert!(blocked_hint(false, false, true).contains("przelej ręcznie"));
    }
}
