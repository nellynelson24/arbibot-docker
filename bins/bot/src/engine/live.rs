use std::time::Duration;

use account::{Bybit, Credentials, Okx, Scope};
use anyhow::{Context, Result, bail};
use serde_json::json;
use tokio::time::interval;
use venues::Venue;

use super::orders::floor_to;
use super::{Accounts, Engine, INVENTORY_SLIPPAGE, LiveArgs, VENUES, http, run};
use crate::inventory::Wallet;
use crate::paper::Side;

const MAX_LIVE_SIZE: f64 = 50.0;
const KEEP_WARM: Duration = Duration::from_secs(15);

pub async fn live(args: LiveArgs) -> Result<()> {
    let common = &args.common;
    for (on, flag) in [
        (common.trade_rebalance, "--trade-rebalance"),
        (common.switch, "--switch"),
    ] {
        if on {
            bail!("{flag} działa na razie tylko na sucho");
        }
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
        bybit: Bybit::new(http, Credentials::load(Venue::Bybit, Scope::Trade)?),
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
        if rights.withdraw {
            bail!(
                "{}: klucz handlowy ma prawo wypłat — usuń go i utwórz nowy tylko z handlem spot",
                venue.name()
            );
        }
        if !rights.trade {
            bail!("{}: klucz nie ma prawa handlu spot", venue.name());
        }
    }
    run(args.common, Some((trade, args.buy_inventory, args.yes))).await
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
    pub(super) async fn resync(&mut self) -> Result<()> {
        if let Err(e) = self.sync_balances().await
            && self.halted.is_none()
        {
            self.halt(format!(
                "nie da się odczytać sald ({e:#}) — sprawdź sieć i klucze"
            ))?;
        }
        Ok(())
    }

    async fn sync_balances(&mut self) -> Result<[Wallet; 2]> {
        let (okx, bybit) =
            tokio::try_join!(self.accounts.okx.balances(), self.accounts.bybit.balances())
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
                "  {:<9} {:.4} {base} (≈{:.2} {quote}) i {:.2} {quote}",
                VENUES[v].name(),
                w.base,
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

        let mut purchases = Vec::new();
        for v in 0..2 {
            let w = self.inventory.wallets[v];
            let spend = (w.quote + w.base * mid) / 2.0 - w.base * mid;
            let Some(ask) = self.books[v].borrow().asks.first().map(|l| l.0) else {
                continue;
            };
            let m = &self.market;

            let qty = floor_to(spend * 0.98 / ask, m.step);
            if spend < self.size || qty < m.min_qty || qty * ask < m.min_notional {
                continue;
            }
            purchases.push((v, qty, ask * (1.0 + INVENTORY_SLIPPAGE)));
        }
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
        let funded = (0..2).any(|d| {
            let w = &self.inventory.wallets;
            w[d].quote >= self.size * 1.01 && w[1 - d].base * mid >= self.size
        });
        if !funded && !(buy_inventory && !purchases.is_empty()) {
            println!(
                "\nUwaga: żaden kierunek nie ma środków na transakcję {} {quote} \
                 ({quote} na jednej giełdzie i {base} na drugiej).",
                self.size
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
                println!(
                    "  {}: kupiono {} {base} po średnio {:.6}",
                    VENUES[v].name(),
                    fill.qty,
                    fill.avg_price
                );
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
