use std::time::{Duration, Instant};

use crate::paper::Side;

const LOW_SHARE: f64 = 0.4;

#[derive(Clone, Copy, Debug, Default)]
pub struct Wallet {
    pub base: f64,
    pub quote: f64,
}

impl Wallet {
    pub fn book(&mut self, side: Side, qty: f64, value: f64, fee: f64) -> f64 {
        let cash = match side {
            Side::Buy => {
                self.base += qty;
                -value * (1.0 + fee)
            }
            Side::Sell => {
                self.base -= qty;
                value * (1.0 - fee)
            }
        };
        self.quote += cash;
        cash
    }
}

pub struct Transfer {
    pub to: usize,
    pub base: f64,
    pub quote: f64,
    pub arrives: Instant,
}

pub struct Rebalance {
    pub base: f64,
    pub quote: f64,
    pub cost: f64,
}

pub struct Inventory {
    pub wallets: [Wallet; 2],
    pub transfers: Vec<Transfer>,
    target: f64,
}

impl Inventory {
    pub fn new(capital: f64, mid: f64) -> Self {
        let part = capital / 4.0;
        let wallet = Wallet {
            base: part / mid,
            quote: part,
        };
        Inventory {
            wallets: [wallet; 2],
            transfers: Vec::new(),
            target: part,
        }
    }

    pub fn equity(&self, mid: f64) -> f64 {
        let held: f64 = self.wallets.iter().map(|w| w.quote + w.base * mid).sum();
        let moving: f64 = self.transfers.iter().map(|t| t.quote + t.base * mid).sum();
        held + moving
    }

    pub fn settle(&mut self) {
        let now = Instant::now();
        let (arrived, moving): (Vec<Transfer>, Vec<Transfer>) =
            self.transfers.drain(..).partition(|t| t.arrives <= now);
        self.transfers = moving;
        for t in arrived {
            self.wallets[t.to].base += t.base;
            self.wallets[t.to].quote += t.quote;
        }
    }

    pub fn rebalance(
        &mut self,
        mid: f64,
        base_fee: [Option<f64>; 2],
        quote_fee: [Option<f64>; 2],
        delay: Duration,
    ) -> Option<Rebalance> {
        let low = |w: &Wallet| {
            w.base * mid < LOW_SHARE * self.target || w.quote < LOW_SHARE * self.target
        };
        if !self.transfers.is_empty() || !self.wallets.iter().any(low) {
            return None;
        }
        let arrives = Instant::now() + delay;
        let mut done = Rebalance {
            base: 0.0,
            quote: 0.0,
            cost: 0.0,
        };
        let [a, b] = self.wallets;
        let from = usize::from(b.base > a.base);
        let amount = (self.wallets[from].base - self.wallets[1 - from].base) / 2.0;
        if let Some(fee) = base_fee[from]
            && amount > fee
        {
            self.wallets[from].base -= amount;
            self.transfers.push(Transfer {
                to: 1 - from,
                base: amount - fee,
                quote: 0.0,
                arrives,
            });
            done.base = amount;
            done.cost += fee * mid;
        }
        let from = usize::from(b.quote > a.quote);
        let amount = (self.wallets[from].quote - self.wallets[1 - from].quote) / 2.0;
        if let Some(fee) = quote_fee[from]
            && amount > fee
        {
            self.wallets[from].quote -= amount;
            self.transfers.push(Transfer {
                to: 1 - from,
                base: 0.0,
                quote: amount - fee,
                arrives,
            });
            done.quote = amount;
            done.cost += fee;
        }
        (done.base > 0.0 || done.quote > 0.0).then_some(done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebalance_moves_half_the_difference_minus_fee() {
        let mut inv = Inventory::new(100.0, 1.0);
        inv.wallets[0].base = 5.0;
        inv.wallets[1].base = 45.0;
        let r = inv
            .rebalance(1.0, [Some(0.1); 2], [Some(0.1); 2], Duration::ZERO)
            .unwrap();
        assert_eq!(r.base, 20.0);
        assert_eq!(inv.wallets[1].base, 25.0);
        inv.settle();
        assert!((inv.wallets[0].base - 24.9).abs() < 1e-12);
    }

    #[test]
    fn no_route_means_no_transfer() {
        let mut inv = Inventory::new(100.0, 1.0);
        inv.wallets[0].base = 1.0;
        assert!(
            inv.rebalance(1.0, [None; 2], [None; 2], Duration::ZERO)
                .is_none()
        );
    }
}
