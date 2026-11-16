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

#[cfg(test)]
mod tests {
    use super::*;

    fn compose_commands(compose: &str) -> Vec<(String, Vec<String>)> {
        let mut out = Vec::new();
        let mut service = String::new();
        let mut lines = compose.lines().peekable();
        while let Some(line) = lines.next() {
            if let Some(name) = line.strip_prefix("  ").and_then(|l| l.strip_suffix(':'))
                && !name.starts_with(' ')
            {
                service = name.to_owned();
            }
            if line.trim() != "command: >-" {
                continue;
            }
            let mut words = Vec::new();
            while let Some(next) = lines.peek().filter(|l| l.starts_with("      ")) {
                words.extend(next.split_whitespace().map(str::to_owned));
                lines.next();
            }
            out.push((service.clone(), words));
        }
        out
    }

    #[test]
    fn every_dry_run_in_compose_parses_and_keeps_its_own_data_dir() {
        let commands = compose_commands(include_str!("../../../compose.yaml"));
        let dry: Vec<&(String, Vec<String>)> = commands
            .iter()
            .filter(|(_, words)| words.get(1).map(String::as_str) == Some("dry-run"))
            .collect();
        for name in [
            "ab-pepe",
            "ab-arb-maker40",
            "ab-arb-rest",
            "ab-usdc-pln",
            "ab-switch",
            "ab-arb-lat",
            "ab-arb-rest-lat",
            "ab-arb-net60",
        ] {
            assert!(dry.iter().any(|(s, _)| s == name), "brak {name}");
        }
        let mut dirs = Vec::new();
        for (service, words) in &dry {
            let parsed = Cli::try_parse_from(words);
            assert!(parsed.is_ok(), "{service}: {:?}", parsed.err());
            let dir = words
                .iter()
                .position(|w| w == "--data-dir")
                .map_or("data".to_owned(), |i| words[i + 1].clone());
            if service.starts_with("ab-") {
                assert_eq!(dir, format!("data/{service}"));
            }
            assert!(!dirs.contains(&dir), "{service}: ten sam katalog {dir}");
            dirs.push(dir);
        }
    }
}
