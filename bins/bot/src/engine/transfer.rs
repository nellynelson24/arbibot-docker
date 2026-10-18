use std::time::{Duration, Instant};

use account::{Bybit, Credentials, DepositAddress, Okx, Scope, Wallet as Account};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::time::sleep;
use venues::Venue;

use super::market::{Route, route_of};
use super::orders::{floor_to, text};
use super::{Accounts, BYBIT, Engine, OKX, VENUES, clock, http};
use crate::inventory::Wallet;
use crate::ledger::now_ms;
use crate::selector;

pub(super) const CHECK_EVERY: Duration = Duration::from_secs(60);
const LOW_SHARE: f64 = 0.4;
const MIN_VALUE: f64 = 10.0;
const DUST_VALUE: f64 = 1.0;
const ALERT_AFTER: Duration = Duration::from_secs(1800);
const PAUSE_AFTER_ERROR: Duration = Duration::from_secs(1800);
const ARRIVAL_TOLERANCE: f64 = 0.03;
const NO_PERMISSION: [&str; 2] = ["50120", "10005"];

pub(super) struct Mover {
    withdraw: Option<Accounts>,
    rcvr_info: Option<Value>,
    routes: [Option<Route>; 2],
    assets: [String; 2],
}

pub(super) struct Sent {
    asset: usize,
    from: usize,
    amount: f64,
    expected: f64,
    since_ms: f64,
    started: Instant,
    alerted: bool,
}

pub(super) struct AutoTransfer {
    mover: Mover,
    in_flight: Option<Sent>,
    day: String,
    today: u32,
    max_per_day: u32,
    max_value: f64,
    paused_until: Option<Instant>,
}

impl AutoTransfer {
    pub(super) fn new(mover: Mover, max_per_day: u32, max_value: f64) -> Self {
        AutoTransfer {
            mover,
            in_flight: None,
            day: selector::utc_date(now_ms()),
            today: 0,
            max_per_day,
            max_value,
            paused_until: None,
        }
    }
}

impl Mover {
    pub(super) async fn new(
        read: &Accounts,
        base: &str,
        quote: &str,
        with_withdraw: bool,
    ) -> Result<Mover> {
        let withdraw = if with_withdraw {
            let client = http()?;
            let keys = Accounts {
                okx: Okx::new(
                    client.clone(),
                    Credentials::load(Venue::Okx, Scope::Withdraw)?,
                ),
                bybit: Bybit::new(client, Credentials::load(Venue::Bybit, Scope::Withdraw)?),
            };
            let rights = [
                keys.okx.key_rights().await.context("klucz wypłat OKX")?,
                keys.bybit
                    .key_rights()
                    .await
                    .context("klucz wypłat Bybit")?,
            ];
            for (venue, rights) in VENUES.iter().zip(rights) {
                if !rights.withdraw {
                    bail!("{}: klucz wypłat nie ma prawa wypłat", venue.name());
                }
            }
            Some(keys)
        } else {
            None
        };
        let rcvr_info = account::secret("federbot-okx-withdraw", "rcvr_info")
            .ok()
            .map(|s| serde_json::from_str(&s))
            .transpose()
            .context("rcvr_info w federbot-okx-withdraw nie jest JSON-em")?;
        Ok(Mover {
            withdraw,
            rcvr_info,
            routes: [route_of(read, base).await?, route_of(read, quote).await?],
            assets: [base.to_owned(), quote.to_owned()],
        })
    }

    fn keys(&self) -> Result<&Accounts> {
        self.withdraw.as_ref().context("brak kluczy wypłat")
    }

    fn route(&self, asset: usize) -> Result<&Route> {
        self.routes[asset].as_ref().with_context(|| {
            format!(
                "{}: brak sieci otwartej w obie strony między OKX a Bybit",
                self.assets[asset]
            )
        })
    }

    async fn destination(
        &self,
        read: &Accounts,
        asset: usize,
        to: usize,
    ) -> Result<DepositAddress> {
        let route = self.route(asset)?;
        let name = &self.assets[asset];
        let list = match to {
            OKX => read.okx.deposit_addresses(name).await?,
            _ => read.bybit.deposit_addresses(name).await?,
        };
        list.into_iter()
            .find(|a| a.chain == route.chains[to])
            .with_context(|| {
                format!(
                    "{}: brak adresu wpłat {name} w sieci {}",
                    VENUES[to].name(),
                    route.chains[to]
                )
            })
    }

    async fn send(
        &self,
        read: &Accounts,
        trade: &Accounts,
        asset: usize,
        from: usize,
        amount: f64,
    ) -> Result<Sent> {
        let keys = self.keys()?;
        let route = self.route(asset)?;
        let name = &self.assets[asset];
        let address = self.destination(read, asset, 1 - from).await?;
        let step = 10f64.powi(-(route.decimals[from] as i32));
        let (fee, min) = (route.fees[from], route.min[from]);
        let id = format!("fbw{}", now_ms() as u64);
        let expected = if from == OKX {
            let amt = floor_to(amount - fee, step);
            if amt < min.max(step) {
                bail!("{name}: {amt} poniżej minimum wypłaty {min}");
            }
            let total = text(amt + fee, step);
            self.okx_transfer(trade, name, &total, Account::Funding)
                .await?;
            let sent = keys
                .okx
                .withdraw(
                    name,
                    &text(amt, step),
                    &route.chains[OKX],
                    &address,
                    &id,
                    self.rcvr_info.as_ref(),
                )
                .await;
            if let Err(e) = sent {
                let back = self
                    .okx_transfer(trade, name, &total, Account::Trading)
                    .await;
                return Err(e.context(match back {
                    Ok(()) => {
                        "wypłata z OKX odrzucona, środki wróciły na konto handlowe".to_owned()
                    }
                    Err(b) => {
                        format!("wypłata z OKX odrzucona, powrót na konto handlowe też: {b:#}")
                    }
                }));
            }
            amt
        } else {
            let amt = floor_to(amount, step);
            if amt < min.max(fee + step) {
                bail!("{name}: {amt} poniżej minimum wypłaty {min}");
            }
            keys.bybit
                .withdraw(name, &route.chains[BYBIT], &address, &text(amt, step), &id)
                .await?;
            amt - fee
        };
        Ok(Sent {
            asset,
            from,
            amount,
            expected,
            since_ms: now_ms(),
            started: Instant::now(),
            alerted: false,
        })
    }

    async fn arrived(&self, read: &Accounts, sent: &Sent) -> Result<bool> {
        let name = &self.assets[sent.asset];
        let deposits = match 1 - sent.from {
            OKX => read.okx.deposits(name).await?,
            _ => read.bybit.deposits(name).await?,
        };
        Ok(deposits.iter().any(|d| {
            d.credited
                && d.ts_ms >= sent.since_ms - 120_000.0
                && (d.amount - sent.expected).abs() <= ARRIVAL_TOLERANCE * sent.expected
        }))
    }

    async fn sweep(&self, read: &Accounts, trade: &Accounts, mid: f64) -> Result<Vec<String>> {
        let (okx, bybit) = tokio::try_join!(read.okx.balances(), read.bybit.balances())?;
        let mut moved = Vec::new();
        for (v, list) in [okx, bybit].iter().enumerate() {
            for (asset, name) in self.assets.iter().enumerate() {
                let free: f64 = list
                    .iter()
                    .filter(|b| b.account == "funding" && b.asset == *name)
                    .map(|b| b.available)
                    .sum();
                let price = if asset == 0 { mid } else { 1.0 };
                if free * price < DUST_VALUE {
                    continue;
                }
                let decimals = self.routes[asset].as_ref().map_or(6, |r| r.decimals[v]);
                let step = 10f64.powi(-(decimals as i32));
                let amount = text(floor_to(free, step), step);
                if v == OKX {
                    self.okx_transfer(trade, name, &amount, Account::Trading)
                        .await?;
                } else {
                    let keys = self.withdraw.as_ref().unwrap_or(trade);
                    match keys
                        .bybit
                        .transfer_to_trading(name, &amount, &transfer_id())
                        .await
                    {
                        Err(e) if denied(&e) => {
                            trade
                                .bybit
                                .transfer_to_trading(name, &amount, &transfer_id())
                                .await?
                        }
                        other => other?,
                    }
                }
                moved.push(format!(
                    "{amount} {name} na {} (funding → trading)",
                    VENUES[v].name()
                ));
            }
        }
        Ok(moved)
    }

    async fn okx_transfer(
        &self,
        trade: &Accounts,
        ccy: &str,
        amount: &str,
        to: Account,
    ) -> Result<()> {
        let keys = self.withdraw.as_ref().unwrap_or(trade);
        match keys.okx.transfer(ccy, amount, to).await {
            Err(e) if denied(&e) => trade.okx.transfer(ccy, amount, to).await,
            other => other,
        }
    }
}

fn denied(e: &anyhow::Error) -> bool {
    let text = format!("{e:#}");
    NO_PERMISSION.iter().any(|code| text.contains(code))
}

fn transfer_id() -> String {
    let n = now_ms() as u64;
    let r = n
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        (n >> 16) as u32,
        n as u16,
        (r >> 52) as u16 & 0xfff,
        (r >> 40) as u16 & 0xfff,
        r & 0xffff_ffff_ffff
    )
}

pub(super) fn plan_move(wallets: [Wallet; 2], mid: f64) -> Option<(usize, usize, f64)> {
    let holdings = [
        [wallets[OKX].base, wallets[BYBIT].base],
        [wallets[OKX].quote, wallets[BYBIT].quote],
    ];
    let price = [mid, 1.0];
    (0..2)
        .filter_map(|asset| {
            let [a, b] = holdings[asset];
            let target = (a + b) / 2.0;
            if target <= 0.0 {
                return None;
            }
            let poor = usize::from(b < a);
            let share = holdings[asset][poor] / target;
            if share >= LOW_SHARE {
                return None;
            }
            let amount = (holdings[asset][1 - poor] - holdings[asset][poor]) / 2.0;
            (amount * price[asset] >= MIN_VALUE).then_some((share, asset, 1 - poor, amount))
        })
        .min_by(|x, y| x.0.total_cmp(&y.0))
        .map(|(_, asset, from, amount)| (asset, from, amount))
}

impl Engine {
    pub(super) async fn transfer_step(&mut self) {
        let Some(mut auto) = self.auto.take() else {
            return;
        };
        if let Err(e) = self.run_transfers(&mut auto).await {
            let line = format!("autorebalans: {e:#} — przerwa na 30 min");
            println!("[{}] {line}", clock());
            self.tell(line);
            auto.paused_until = Some(Instant::now() + PAUSE_AFTER_ERROR);
        }
        self.auto = Some(auto);
        self.last_tick_ms = now_ms();
    }

    async fn run_transfers(&mut self, auto: &mut AutoTransfer) -> Result<()> {
        let Some(mid) = self.mid() else {
            return Ok(());
        };
        if self.live.is_none()
            || self.halted.is_some()
            || auto.paused_until.is_some_and(|t| Instant::now() < t)
        {
            return Ok(());
        }
        let today = selector::utc_date(now_ms());
        if today != auto.day {
            auto.day = today;
            auto.today = 0;
        }
        let trade = self.live.as_ref().expect("checked above");
        let moved = auto.mover.sweep(&self.accounts, trade, mid).await?;
        let mut changed = !moved.is_empty();
        for line in moved {
            self.tell(format!("autorebalans: {line}"));
        }
        if let Some(sent) = &mut auto.in_flight {
            let name = auto.mover.assets[sent.asset].clone();
            if auto.mover.arrived(&self.accounts, sent).await? {
                let line = format!(
                    "autorebalans: dotarło {:.4} {name} na {} po {} min",
                    sent.expected,
                    VENUES[1 - sent.from].name(),
                    sent.started.elapsed().as_secs() / 60
                );
                let expected = sent.expected;
                auto.in_flight = None;
                println!("[{}] {line}", clock());
                self.tell(line);
                self.ledger.write(json!({
                    "type": "transfer_arrived",
                    "asset": name,
                    "amount": expected,
                }))?;
                let trade = self.live.as_ref().expect("checked above");
                for line in auto.mover.sweep(&self.accounts, trade, mid).await? {
                    self.tell(format!("autorebalans: {line}"));
                }
                changed = true;
            } else if !sent.alerted && sent.started.elapsed() > ALERT_AFTER {
                sent.alerted = true;
                let line = format!(
                    "autorebalans: przelew {:.4} {name} z {} idzie już ponad 30 min — sprawdź na giełdach",
                    sent.amount,
                    VENUES[sent.from].name()
                );
                self.tell(line);
            }
        } else if auto.today < auto.max_per_day
            && let Some((asset, from, amount)) = plan_move(self.free(), mid)
        {
            let price = if asset == 0 { mid } else { 1.0 };
            let amount = amount.min(auto.max_value / price);
            let name = auto.mover.assets[asset].clone();
            let trade = self.live.as_ref().expect("checked above");
            let sent = auto
                .mover
                .send(&self.accounts, trade, asset, from, amount)
                .await?;
            let line = format!(
                "autorebalans: przelew {amount:.4} {name} z {} na {} (dotrze ok. {:.4})",
                VENUES[from].name(),
                VENUES[1 - from].name(),
                sent.expected
            );
            println!("[{}] {line}", clock());
            self.tell(line);
            self.ledger.write(json!({
                "type": "transfer",
                "asset": name,
                "from": VENUES[from].id(),
                "amount": amount,
                "expected": sent.expected,
            }))?;
            auto.today += 1;
            auto.in_flight = Some(sent);
            changed = true;
        }
        if changed {
            self.resync().await?;
        }
        Ok(())
    }
}

#[derive(clap::Args)]
pub struct TransferArgs {
    #[arg(long, default_value = "TIA/USDC")]
    #[arg(help = "Para bota: sieci i adresy dla jej monety i waluty")]
    pair: String,
    #[arg(long)]
    #[arg(help = "Co przelać, np. TIA albo USDC")]
    asset: String,
    #[arg(long)]
    #[arg(help = "Skąd: okx albo bybit")]
    from: String,
    #[arg(long, default_value_t = 0.0)]
    #[arg(help = "Ile, w jednostkach przelewanego aktywa")]
    amount: f64,
    #[arg(long)]
    #[arg(
        help = "Tylko pokaż trasę, opłatę i adres; nic nie wysyłaj (wystarczą klucze do odczytu)"
    )]
    plan: bool,
    #[arg(long)]
    #[arg(help = "Bez pytania o potwierdzenie")]
    yes: bool,
}

pub async fn transfer(args: TransferArgs) -> Result<()> {
    let (base, quote) = args
        .pair
        .to_uppercase()
        .split_once('/')
        .map(|(b, q)| (b.to_owned(), q.to_owned()))
        .context("para w formacie BAZA/KWOTOWANIE")?;
    let name = args.asset.to_uppercase();
    let asset = if name == base {
        0
    } else if name == quote {
        1
    } else {
        bail!("{name} nie należy do pary {base}/{quote}");
    };
    let from = match args.from.to_lowercase().as_str() {
        "okx" => OKX,
        "bybit" => BYBIT,
        other => bail!("nieznana giełda {other}: okx albo bybit"),
    };
    let client = http()?;
    let read = Accounts {
        okx: Okx::new(client.clone(), Credentials::load(Venue::Okx, Scope::Read)?),
        bybit: Bybit::new(
            client.clone(),
            Credentials::load(Venue::Bybit, Scope::Read)?,
        ),
    };
    let mover = Mover::new(&read, &base, &quote, !args.plan).await?;
    let route = mover.route(asset)?.clone();
    let to = 1 - from;
    let address = mover.destination(&read, asset, to).await?;
    let a = &address.address;
    println!(
        "Trasa {name}: {} → {}, sieć {} / {}, opłata {} {name}, minimum {} {name}, adres {}…{}{}",
        VENUES[from].name(),
        VENUES[to].name(),
        route.chains[OKX],
        route.chains[BYBIT],
        route.fees[from],
        route.min[from],
        &a[..a.len().min(8)],
        &a[a.len().saturating_sub(6)..],
        if address.tag.is_some() { " z memo" } else { "" }
    );
    if args.plan {
        return Ok(());
    }
    if args.amount <= 0.0 {
        bail!("podaj --amount");
    }
    let trade = Accounts {
        okx: Okx::new(client.clone(), Credentials::load(Venue::Okx, Scope::Trade)?),
        bybit: Bybit::new(client, Credentials::load(Venue::Bybit, Scope::Trade)?),
    };
    if !args.yes {
        println!(
            "Wysłać {} {name}? Wpisz TAK i Enter (cokolwiek innego kończy):",
            args.amount
        );
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if line.trim() != "TAK" {
            bail!("nie potwierdzono — nic nie wysłano");
        }
    }
    let sent = mover.send(&read, &trade, asset, from, args.amount).await?;
    println!("Wysłano. Czekam na wpływ ok. {:.6} {name}…", sent.expected);
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(3600) {
        if mover.arrived(&read, &sent).await? {
            println!("Dotarło po {} s.", started.elapsed().as_secs());
            for line in mover.sweep(&read, &trade, 1.0).await? {
                println!("Przeniesiono {line}");
            }
            return Ok(());
        }
        sleep(Duration::from_secs(20)).await;
    }
    bail!("przelew nie dotarł w ciągu godziny — sprawdź na giełdach")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wallet(base: f64, quote: f64) -> Wallet {
        Wallet { base, quote }
    }

    #[test]
    fn moves_half_the_gap_of_the_scarcer_asset() {
        assert_eq!(
            plan_move([wallet(50.0, 50.0), wallet(50.0, 50.0)], 1.0),
            None
        );
        assert_eq!(
            plan_move([wallet(10.0, 50.0), wallet(90.0, 50.0)], 1.0),
            Some((0, BYBIT, 40.0))
        );
        assert_eq!(
            plan_move([wallet(50.0, 95.0), wallet(50.0, 5.0)], 1.0),
            Some((1, OKX, 45.0))
        );
        assert_eq!(plan_move([wallet(1.0, 50.0), wallet(9.0, 50.0)], 1.0), None);
    }

    #[test]
    fn transfer_ids_look_like_uuids() {
        let id = transfer_id();
        assert_eq!(id.len(), 36);
        assert_eq!(id.matches('-').count(), 4);
    }
}
