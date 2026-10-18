mod book;
mod check;
mod edge;
mod engine;
mod inventory;
mod ledger;
mod paper;
mod report;
mod selector;
mod telegram;

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(about = "Bot handlowy taker–taker OKX EEA ↔ Bybit EU (plan: BOT_PLAN.md)", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "Faza 1: odczyt kont — salda, prowizje, reguły, sieci wypłat", long_about = None)]
    Check(check::Args),
    #[command(about = "Na żywo: zysk po prowizjach dla kilku kwot, z głębokości książek obu giełd", long_about = None)]
    Edge(edge::Args),
    #[command(about = "Faza 2: cały bot bez wysyłania zleceń (symulowane wypełnienia, salda, rebalans)", long_about = None)]
    DryRun(engine::Args),
    #[command(about = "Faza 4: handel prawdziwymi pieniędzmi (klucze handlowe bez prawa wypłat)", long_about = None)]
    Live(engine::LiveArgs),
    #[command(about = "Telegram: konfiguracja i wiadomość testowa (token i czat w pęku kluczy)", long_about = None)]
    Telegram(telegram::Args),
    #[command(about = "Raport: handel na żywo, wszystkie dry-runy i rynek ze skanera, z wnioskami", long_about = None)]
    Report(report::Args),
    #[command(about = "Przelew monety albo waluty między giełdami (z --plan tylko pokazuje trasę)", long_about = None)]
    Transfer(engine::TransferArgs),
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow!("nie udało się ustawić dostawcy kryptografii rustls"))?;

    match Cli::parse().command {
        Command::Check(args) => check::run(args).await,
        Command::Edge(args) => edge::run(args).await,
        Command::DryRun(args) => engine::dry_run(args).await,
        Command::Live(args) => engine::live(args).await,
        Command::Telegram(args) => telegram::setup(args).await,
        Command::Report(args) => report::run(args).await,
        Command::Transfer(args) => engine::transfer(args).await,
    }
}
