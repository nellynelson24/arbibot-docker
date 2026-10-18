use std::path::PathBuf;

#[derive(clap::Args)]
pub struct Args {
    #[arg(long)]
    #[arg(help = "Para, np. STRK/USDC; domyślnie najlepsza ze skanera")]
    pub(super) pair: Option<String>,
    #[arg(long, default_value_t = 100.0)]
    #[arg(help = "Na sucho: kapitał (waluta kwotowania). Na żywo liczą się prawdziwe salda")]
    pub(super) capital: f64,
    #[arg(long, default_value_t = 10.0)]
    #[arg(help = "Wielkość transakcji (waluta kwotowania)")]
    pub(super) size: f64,
    #[arg(long, default_value_t = 30.0, allow_negative_numbers = true)]
    #[arg(help = "Próg zysku po prowizjach (bp; 1 bp = 0,01%)")]
    pub(super) min_bps: f64,
    #[arg(long, default_value_t = 0.0)]
    #[arg(help = "Zapas ceny na nogę (bp), najwyżej ćwierć oczekiwanego zysku")]
    pub(super) slack_bps: f64,
    #[arg(long)]
    #[arg(
        help = "Nogi po kolei: najpierw giełda, której cena dłużej stoi, druga po jej wypełnieniu z limitem bez straty"
    )]
    pub(super) sequential: bool,
    #[arg(long)]
    #[arg(help = "Rebalans handlem, gdy kierunek jest zablokowany od 15 min (zamiast przelewów)")]
    pub(super) trade_rebalance: bool,
    #[arg(long, default_value_t = 30.0, allow_negative_numbers = true)]
    #[arg(
        help = "Próg dla kierunku, który wyrównuje salda, gdy przeciwny ma mniej niż połowę jego środków (bp)"
    )]
    pub(super) restore_min_bps: f64,
    #[arg(long, default_value_t = 0.0)]
    #[arg(
        help = "Przechył progów według sald (bp): kierunek wyrównujący salda tańszy, rozjeżdżający droższy, także dla makera"
    )]
    pub(super) skew_bps: f64,
    #[arg(long)]
    #[arg(
        help = "Płatny rebalans handlem tylko po sygnale straconym z braku środków, najwyżej raz na 30 min i 4 razy na dobę"
    )]
    pub(super) lazy_rebalance: bool,
    #[arg(long, default_value_t = 45.0)]
    #[arg(help = "Najwyższy koszt rebalansu handlem (bp)")]
    pub(super) rebalance_max_cost_bps: f64,
    #[arg(long)]
    #[arg(
        help = "Na żywo: automatyczne przelewy monety i waluty między giełdami (klucze wypłat federbot-<giełda>-withdraw)"
    )]
    pub(super) auto_transfer: bool,
    #[arg(long, default_value_t = 6)]
    #[arg(help = "Najwięcej automatycznych przelewów na dobę")]
    pub(super) max_transfers: u32,
    #[arg(long, default_value_t = 100.0)]
    #[arg(help = "Najwyższa wartość jednego automatycznego przelewu (waluta kwotowania)")]
    pub(super) max_transfer_value: f64,
    #[arg(long, default_value_t = 100)]
    #[arg(help = "Na sucho: opóźnienie zleceń na OKX (ms)")]
    pub(super) okx_latency_ms: u64,
    #[arg(long, default_value_t = 200)]
    #[arg(help = "Na sucho: opóźnienie zleceń na Bybit (ms)")]
    pub(super) bybit_latency_ms: u64,
    #[arg(long, default_value_t = 2.0)]
    #[arg(help = "Dzienny limit straty (waluta kwotowania)")]
    pub(super) daily_loss_limit: f64,
    #[arg(long, default_value_t = 10)]
    #[arg(help = "Na sucho: czas przelewu przy rebalansie (min)")]
    pub(super) transfer_mins: u64,
    #[arg(long, default_value = "data")]
    #[arg(help = "Katalog danych")]
    pub(super) data_dir: PathBuf,
    #[arg(long)]
    #[arg(help = "Na sucho: automatyczna zmiana pary")]
    pub(super) switch: bool,
    #[arg(long, default_value_t = 24.0)]
    #[arg(help = "Co ile godzin oceniać pary (z --switch)")]
    pub(super) review_hours: f64,
    #[arg(long)]
    #[arg(help = "Powiadomienia i komendy /stan, /stop przez Telegram")]
    pub(super) telegram: bool,
    #[arg(long)]
    #[arg(help = "Zlecenia maker na OKX zabezpieczane na Bybit")]
    pub(super) maker: bool,
    #[arg(long)]
    #[arg(help = "Wielkość zlecenia maker (waluta kwotowania); domyślnie --size")]
    pub(super) maker_size: Option<f64>,
    #[arg(long, default_value_t = 10.0, allow_negative_numbers = true)]
    #[arg(help = "Minimalna marża maker (bp)")]
    pub(super) maker_min_bps: f64,
    #[arg(long, default_value_t = 1_000)]
    #[arg(help = "Minimalny odstęp przestawień zlecenia maker (ms)")]
    pub(super) requote_ms: u64,
}

#[derive(clap::Args)]
pub struct LiveArgs {
    #[command(flatten)]
    pub(super) common: Args,
    #[arg(long)]
    #[arg(help = "Na starcie dokup brakującą monetę")]
    pub(super) buy_inventory: bool,
    #[arg(long)]
    #[arg(help = "Bez pytania o potwierdzenie")]
    pub(super) yes: bool,
}
