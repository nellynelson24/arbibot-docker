use std::time::Duration;

use account::{Bybit, Credentials, KeyRights, Okx, Scope};
use anyhow::{Context, Result, bail};
use serde_json::json;
use tokio::time::{interval, sleep};
use venues::Venue;

use super::orders::floor_to;
use super::totals::signed;
use super::{
    Accounts, Engine, Holding, INVENTORY_SLIPPAGE, LiveArgs, LiveStart, SIZE_MARGIN, VENUES,
    amount, clock, http, run,
};
use crate::inventory::Wallet;
use crate::paper::Side;

const MAX_LIVE_SIZE: f64 = 50.0;
const OUTSIDE_CHANGE: f64 = 1.0;
const BALANCE_ATTEMPTS: u32 = 3;
const BALANCE_RETRY: Duration = Duration::from_secs(2);
const KEEP_WARM: Duration = Duration::from_secs(15);

pub async fn live(args: LiveArgs) -> Result<()> {
    let common = &args.common;
    if common.switch {
        bail!("--switch działa na razie tylko na sucho");
    }
    for size in [Some(common.size), common.maker_size].into_iter().flatten() {
        if size > MAX_LIVE_SIZE {
            bail!("na żywo zlecenie może mieć najwyżej {MAX_LIVE_SIZE} (podano {size})");
        }
    }
    let http = http()?;
    keep_warm(http.clone());
    let trade = Accounts {
        okx: Okx::new(http.clone(), Credentials::load(Venue::Okx, Scope::Trade)?),
        bybit: Bybit::new(http.clone(), Credentials::load(Venue::Bybit, Scope::Trade)?),
    };
    let rights = [
        trade.okx.key_rights().await.context("klucz handlowy OKX")?,
        trade
            .bybit
            .key_rights()
            .await
            .context("klucz handlowy Bybit")?,
    ];
    for (venue, rights) in VENUES.iter().zip(rights) {
        check_key(venue.name(), &rights, true)?;
    }
    let read = [
        Okx::new(http.clone(), Credentials::load(Venue::Okx, Scope::Read)?)
            .key_rights()
            .await,
        Bybit::new(http.clone(), Credentials::load(Venue::Bybit, Scope::Read)?)
            .key_rights()
            .await,
    ];
    for (venue, rights) in VENUES.iter().zip(read) {
        match rights {
            Ok(rights) => check_key(venue.name(), &rights, false)?,
            Err(e) => println!(
                "{}: nie da się sprawdzić praw klucza do odczytu ({e:#})",
                venue.name()
            ),
        }
    }
    run(
        args.common,
        Some(LiveStart {
            accounts: trade,
            buy_inventory: args.buy_inventory,
            confirmed: args.yes,
            hedge_leftovers: args.hedge_leftovers,
        }),
    )
    .await
}

fn keep_warm(http: reqwest::Client) {
    tokio::spawn(async move {
        let mut every = interval(KEEP_WARM);
        loop {
            every.tick().await;
            let get = |url: &'static str| {
                let http = http.clone();
                async move {
                    if let Ok(response) = http.get(url).send().await {
                        let _ = response.bytes().await;
                    }
                }
            };
            tokio::join!(get(Okx::TIME_URL), get(Bybit::TIME_URL));
        }
    });
}

impl Engine {
    fn purchases(&self, mid: f64) -> Vec<(usize, f64, f64)> {
        let mut purchases = Vec::new();
        for v in 0..2 {
            let w = self.inventory.wallets[v];
            let spend = (w.quote + w.base * mid) / 2.0 - w.base * mid;
            let Some(ask) = self.books[v].borrow().asks.first().map(|l| l.0) else {
                continue;
            };
            let m = &self.market;
            if let Some(qty) = purchase_qty(
                spend,
                ask,
                self.min_trade(),
                m.step,
                m.min_qty,
                m.min_notional,
            ) {
                purchases.push((v, qty, ask * (1.0 + INVENTORY_SLIPPAGE)));
            }
        }
        purchases
    }

    pub(super) async fn resync(&mut self) -> Result<()> {
        let mid = self.mid();
        let booked = mid.map(|mid| self.inventory.equity(mid));
        match self.sync_balances().await {
            Ok(_) => {
                if let (Some(mid), Some(booked)) = (mid, booked) {
                    self.note_outside_change(self.inventory.equity(mid) - booked)?;
                }
            }
            Err(e) if self.halted.is_none() => {
                self.halt(format!(
                    "nie da się odczytać sald ({e:#}) — sprawdź sieć i klucze"
                ))?;
            }
            Err(_) => {}
        }
        Ok(())
    }

    fn note_outside_change(&mut self, change: f64) -> Result<()> {
        if self.auto.is_some() || change.abs() < OUTSIDE_CHANGE {
            return Ok(());
        }
        self.stats.outside_change += change;
        self.totals.outside_change(change);
        let line = format!(
            "Salda na giełdach zmieniły się poza botem o {} {} (wpłata, wypłata albo ręczna \
             transakcja). Bot liczy to osobno, nie jako zysk ani zmianę cen.",
            signed(change, 2),
            self.market.quote
        );
        println!("[{}] {line}", clock());
        self.tell(line);
        self.ledger
            .write(json!({"type": "outside_change", "value": change}))
    }

    async fn sync_balances(&mut self) -> Result<[Wallet; 2]> {
        let accounts = &self.accounts;
        let (okx, bybit) = retry(BALANCE_ATTEMPTS, BALANCE_RETRY, || async {
            tokio::try_join!(accounts.okx.balances(), accounts.bybit.balances())
        })
        .await
        .context("odczyt sald")?;
        let m = &self.market;
        let mut funding = [Wallet::default(); 2];
        for (v, list) in [okx, bybit].iter().enumerate() {
            let sum = |account: &str, asset: &str| -> f64 {
                list.iter()
                    .filter(|b| b.account == account && b.asset == asset)
                    .map(|b| match account {
                        "trading" => b.total,
                        _ => b.available,
                    })
                    .sum()
            };
            self.inventory.wallets[v] = Wallet {
                base: sum("trading", &m.base),
                quote: sum("trading", &m.quote),
            };
            funding[v] = Wallet {
                base: sum("funding", &m.base),
                quote: sum("funding", &m.quote),
            };
            self.outside[v] = list
                .iter()
                .filter(|b| {
                    b.total > 0.0
                        && (b.account == "funding" || (b.asset != m.base && b.asset != m.quote))
                })
                .map(|b| Holding {
                    account: b.account,
                    asset: b.asset.clone(),
                    qty: b.total,
                    usd: b.usd,
                })
                .collect();
        }
        Ok(funding)
    }

    pub(super) async fn prepare_live(
        &mut self,
        mid: f64,
        buy_inventory: bool,
        confirmed: bool,
    ) -> Result<()> {
        let funding = self.sync_balances().await?;
        let (base, quote) = (self.market.base.clone(), self.market.quote.clone());
        println!("\nSalda na kontach handlowych:");
        for v in 0..2 {
            let w = self.inventory.wallets[v];
            println!(
                "  {:<9} {} {base} (≈{:.2} {quote}) i {:.2} {quote}",
                VENUES[v].name(),
                amount(w.base, 4),
                w.base * mid,
                w.quote
            );
            let f = funding[v];
            if f.base > 0.0 || f.quote > 0.0 {
                println!(
                    "            na koncie funding: {:.4} {base} i {:.2} {quote} — bot ich nie widzi; \
                     przenieś na konto handlowe w aplikacji",
                    f.base, f.quote
                );
            }
        }

        let purchases = self.purchases(mid);
        if !purchases.is_empty() {
            println!("\nBrakuje {base} do sprzedaży:");
            for &(v, qty, limit) in &purchases {
                println!(
                    "  {}: kupno {qty} {base} po najwyżej {limit:.6} (≈{:.2} {quote}){}",
                    VENUES[v].name(),
                    qty * limit,
                    if buy_inventory {
                        ""
                    } else {
                        " — uruchom z --buy-inventory, żeby bot kupił"
                    }
                );
            }
        }
        let funded = funded(&self.inventory.wallets, mid, self.min_trade());
        if !funded && !(buy_inventory && !purchases.is_empty()) {
            println!(
                "\nUwaga: żaden kierunek nie ma środków na transakcję od {:.2} {quote} \
                 ({quote} na jednej giełdzie i {base} na drugiej).",
                self.min_trade()
            );
        }

        println!(
            "\nBot będzie składał PRAWDZIWE zlecenia na {base}/{quote}: transakcja {} {quote}, \
             próg {} bp, dzienny limit straty {} {quote}.",
            self.size, self.min_bps, self.daily_loss_limit
        );
        if !confirmed {
            println!("Wpisz TAK i Enter, żeby zacząć (cokolwiek innego kończy):");
            let answer = tokio::task::spawn_blocking(|| {
                let mut line = String::new();
                std::io::stdin().read_line(&mut line).map(|_| line)
            })
            .await??;
            if answer.trim() != "TAK" {
                bail!("nie potwierdzono — koniec bez zleceń");
            }
        }

        if buy_inventory {
            for (v, qty, limit) in purchases {
                let fill = self.ioc(v, Side::Buy, qty, limit).await?;
                let line = format!(
                    "{}: kupiono {} {base} po średnio {:.6} (≈{:.2} {quote})",
                    VENUES[v].name(),
                    fill.qty,
                    fill.avg_price,
                    fill.qty * fill.avg_price
                );
                println!("  {line}");
                self.tell(line);
                self.ledger.write(json!({
                    "type": "inventory_buy",
                    "venue": VENUES[v].id(),
                    "qty": qty,
                    "fill": [fill.qty, fill.avg_price],
                    "orders": self.take_orders(),
                }))?;
            }
            self.sync_balances().await?;
        }
        Ok(())
    }
}

fn check_key(venue: &str, rights: &KeyRights, trading: bool) -> Result<()> {
    let kind = if trading { "handlowy" } else { "do odczytu" };
    if rights.withdraw {
        bail!("{venue}: klucz {kind} ma prawo wypłat — usuń go i utwórz nowy bez wypłat");
    }
    if trading && !rights.trade {
        bail!("{venue}: klucz nie ma prawa handlu spot");
    }
    Ok(())
}

async fn retry<T, F, Fut>(attempts: u32, delay: Duration, mut call: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut tries = 1;
    loop {
        match call().await {
            Ok(value) => return Ok(value),
            Err(e) if tries >= attempts => return Err(e),
            Err(e) => {
                println!("odczyt nieudany ({e:#}), ponawiam");
                tries += 1;
                sleep(delay).await;
            }
        }
    }
}

fn purchase_qty(
    spend: f64,
    ask: f64,
    min_trade: f64,
    step: f64,
    min_qty: f64,
    min_notional: f64,
) -> Option<f64> {
    let qty = floor_to(spend * 0.98 / ask, step);
    (spend >= min_trade && qty >= min_qty && qty * ask >= min_notional).then_some(qty)
}

fn funded(wallets: &[Wallet; 2], mid: f64, min_trade: f64) -> bool {
    (0..2).any(|d| {
        wallets[d].quote >= min_trade * (1.0 + SIZE_MARGIN)
            && wallets[1 - d].base * mid >= min_trade
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_with_withdrawal_rights_are_refused() {
        let rights = |trade, withdraw| KeyRights { trade, withdraw };
        assert!(check_key("OKX EEA", &rights(true, false), true).is_ok());
        assert!(check_key("OKX EEA", &rights(false, false), false).is_ok());
        assert!(check_key("OKX EEA", &rights(false, true), false).is_err());
        assert!(check_key("OKX EEA", &rights(true, true), true).is_err());
        assert!(check_key("OKX EEA", &rights(false, false), true).is_err());
    }

    #[tokio::test]
    async fn a_read_that_fails_once_is_retried() {
        let calls = std::cell::Cell::new(0);
        let value = retry(3, Duration::ZERO, || {
            calls.set(calls.get() + 1);
            let n = calls.get();
            async move {
                if n == 1 {
                    anyhow::bail!("timeout")
                }
                Ok(n)
            }
        })
        .await
        .unwrap();
        assert_eq!(value, 2);
        let failing: Result<u32> = retry(3, Duration::ZERO, || async { anyhow::bail!("x") }).await;
        assert!(failing.is_err());
    }

    #[test]
    fn buys_from_a_quarter_of_the_trade_size() {
        let qty = purchase_qty(67.0, 1.5, 10.0, 0.01, 0.1, 1.0);
        assert_eq!(qty, Some(43.77));
        assert_eq!(purchase_qty(7.8, 1.5, 10.0, 0.01, 0.1, 1.0), None);
        assert_eq!(purchase_qty(12.0, 1.5, 10.0, 0.01, 0.1, 20.0), None);
    }

    #[test]
    fn small_balance_on_one_venue_still_funds_one_direction() {
        let wallets = [
            Wallet {
                base: 0.0,
                quote: 15.63,
            },
            Wallet {
                base: 45.0,
                quote: 69.0,
            },
        ];
        assert!(funded(&wallets, 1.5, 10.0));
        let empty = [
            Wallet {
                base: 0.0,
                quote: 9.0,
            },
            Wallet {
                base: 0.0,
                quote: 69.0,
            },
        ];
        assert!(!funded(&empty, 1.5, 10.0));
    }
}
