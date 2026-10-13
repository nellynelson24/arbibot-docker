use std::time::Duration;

use tokio::sync::watch;
use tokio::time::sleep;

use crate::book::Book;

pub use account::Side;

#[derive(Clone, Copy, Debug, Default)]
pub struct Fill {
    pub qty: f64,
    pub avg_price: f64,
}

impl Fill {
    pub fn value(&self) -> f64 {
        self.qty * self.avg_price
    }
}

pub async fn ioc(
    book: watch::Receiver<Book>,
    side: Side,
    qty: f64,
    limit: f64,
    latency: Duration,
) -> Fill {
    sleep(latency).await;
    let book = book.borrow().clone();
    match side {
        Side::Buy => match_levels(&book.asks, qty, |price| price <= limit),
        Side::Sell => match_levels(&book.bids, qty, |price| price >= limit),
    }
}

fn match_levels(levels: &[(f64, f64)], qty: f64, acceptable: impl Fn(f64) -> bool) -> Fill {
    let (mut filled, mut cost) = (0.0, 0.0);
    for &(price, size) in levels {
        if filled >= qty || !acceptable(price) {
            break;
        }
        let take = (qty - filled).min(size);
        filled += take;
        cost += take * price;
    }
    Fill {
        qty: filled,
        avg_price: if filled > 0.0 { cost / filled } else { 0.0 },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioc_stops_at_limit_and_cancels_the_rest() {
        let asks = vec![(1.0, 5.0), (1.1, 5.0), (1.2, 5.0)];
        let fill = match_levels(&asks, 12.0, |p| p <= 1.1);
        assert_eq!(fill.qty, 10.0);
        assert!((fill.avg_price - 1.05).abs() < 1e-12);
    }
}
