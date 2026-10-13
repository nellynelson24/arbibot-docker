use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use account::OrderFill;
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use tokio::time::sleep;
use venues::Venue;

use super::{Accounts, Engine, Market, VENUES};
use crate::ledger::now_ms;
use crate::paper::{self, Fill, Side};

const ORDER_TIMEOUT: Duration = Duration::from_secs(10);
const ORDER_POLL: Duration = Duration::from_millis(100);

impl Accounts {
    async fn place_ioc(
        &self,
        venue: Venue,
        m: &Market,
        side: Side,
        qty: &str,
        price: &str,
        id: &str,
    ) -> Result<()> {
        match venue {
            Venue::Okx => {
                self.okx
                    .place_ioc(&m.base, &m.quote, side, qty, price, id)
                    .await
            }
            _ => {
                self.bybit
                    .place_ioc(&m.base, &m.quote, side, qty, price, id)
                    .await
            }
        }
    }

    async fn order(&self, venue: Venue, m: &Market, id: &str) -> Result<Option<OrderFill>> {
        match venue {
            Venue::Okx => self.okx.order(&m.base, &m.quote, id).await,
            _ => self.bybit.order(&m.base, &m.quote, id).await,
        }
    }
}

impl Engine {
    pub(super) async fn ioc(&self, v: usize, side: Side, qty: f64, limit: f64) -> Result<Fill> {
        let Some(trade) = &self.live else {
            return Ok(paper::ioc(self.books[v].clone(), side, qty, limit, self.latency[v]).await);
        };
        let (m, venue) = (&self.market, VENUES[v]);
        let qty = text(floor_to(qty, m.step), m.step);
        let price = match side {
            Side::Buy => floor_to(limit, m.tick[v]),
            Side::Sell => ceil_to(limit, m.tick[v]),
        };
        let price = text(price, m.tick[v]);
        let id = format!(
            "fb{}x{}",
            now_ms() as u64,
            self.order_seq.fetch_add(1, Ordering::Relaxed)
        );
        let sent = Instant::now();
        let placed = trade.place_ioc(venue, m, side, &qty, &price, &id).await;
        let ack_ms = sent.elapsed().as_secs_f64() * 1000.0;

        let timeout = if placed.is_ok() {
            ORDER_TIMEOUT
        } else {
            Duration::from_secs(2)
        };
        let started = Instant::now();
        loop {
            if let Ok(Some(fill)) = trade.order(venue, m, &id).await {
                self.log_order(json!({
                    "venue": venue.id(),
                    "id": id,
                    "side": side_id(side),
                    "qty": qty,
                    "price": price,
                    "ack_ms": ack_ms,
                    "filled": fill.qty,
                    "avg": fill.avg_price,
                }));
                return Ok(Fill {
                    qty: fill.qty,
                    avg_price: fill.avg_price,
                });
            }
            if started.elapsed() > timeout {
                self.log_order(json!({
                    "venue": venue.id(),
                    "id": id,
                    "qty": qty,
                    "price": price,
                    "ack_ms": ack_ms,
                    "error": placed.as_ref().err().map(|e| format!("{e:#}")),
                }));
                let what = match side {
                    Side::Buy => "kupna",
                    Side::Sell => "sprzedaży",
                };
                return Err(match placed {
                    Err(e) => e.context(format!(
                        "{}: zlecenie {what} {qty} po {price}",
                        venue.name()
                    )),
                    Ok(()) => anyhow!(
                        "{}: zlecenie {id} bez wyniku po {} s",
                        venue.name(),
                        timeout.as_secs()
                    ),
                });
            }
            sleep(ORDER_POLL).await;
        }
    }

    fn log_order(&self, order: Value) {
        if let Ok(mut orders) = self.orders.lock() {
            orders.push(order);
        }
    }

    pub(super) fn take_orders(&self) -> Vec<Value> {
        self.orders
            .lock()
            .map(|mut orders| std::mem::take(&mut *orders))
            .unwrap_or_default()
    }
}

pub(super) fn side_id(side: Side) -> &'static str {
    match side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}

pub(super) fn floor_to(x: f64, step: f64) -> f64 {
    (x / step + 1e-9).floor() * step
}

pub(super) fn ceil_to(x: f64, step: f64) -> f64 {
    (x / step - 1e-9).ceil() * step
}

pub(super) fn text(x: f64, step: f64) -> String {
    let decimals = format!("{step}")
        .split_once('.')
        .map_or(0, |(_, fraction)| fraction.len());
    format!("{x:.decimals$}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_numbers_follow_the_venue_steps() {
        assert_eq!(text(floor_to(95.49999999999999, 0.1), 0.1), "95.5");
        assert_eq!(text(floor_to(95.4999, 0.1), 0.1), "95.4");
        assert_eq!(text(floor_to(95.57, 0.1), 0.1), "95.5");
        assert_eq!(text(floor_to(176.0, 1.0), 1.0), "176");
        assert_eq!(text(floor_to(0.10462 * 1.01, 0.00001), 0.00001), "0.10566");
        assert_eq!(text(ceil_to(0.10462 * 0.99, 0.00001), 0.00001), "0.10358");
        assert_eq!(text(ceil_to(0.1046, 0.0001), 0.0001), "0.1046");
    }
}
