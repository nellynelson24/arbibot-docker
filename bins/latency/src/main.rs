mod stats;
mod venue;

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use futures_util::{SinkExt, StreamExt, future::join_all};
use tokio::time::{interval, sleep, timeout};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use stats::Summary;
use venue::{Feed, Ping, Venue};

const REST_INTERVAL: Duration = Duration::from_millis(250);
const WS_PING_INTERVAL: Duration = Duration::from_millis(200);
const WS_TIMEOUT: Duration = Duration::from_secs(5);

const WS_KEEPALIVE: Duration = Duration::from_secs(15);

#[derive(Parser)]
#[command(about = "Pomiar opóźnień do giełd: REST, WebSocket i opóźnienie strumienia książki zleceń", long_about = None)]
struct Args {
    #[arg(long, value_enum, value_delimiter = ',')]
    #[arg(help = "Giełdy do zmierzenia, np. okx,kraken (domyślnie wszystkie)")]
    venues: Vec<Venue>,
    #[arg(long, default_value = "XRP")]
    #[arg(help = "Waluta bazowa")]
    base: String,
    #[arg(long, default_value = "EUR")]
    #[arg(help = "Waluta kwotowania (EUR jest na wszystkich giełdach)")]
    quote: String,
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..))]
    #[arg(help = "Liczba próbek REST i ping WebSocket")]
    samples: u32,
    #[arg(long, default_value_t = 30)]
    #[arg(help = "Czas nasłuchu strumienia książki zleceń (sekundy)")]
    feed_secs: u64,
}

struct Report {
    symbol: String,
    rest: RestReport,
    ws: WsReport,
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow!("nie udało się ustawić dostawcy kryptografii rustls"))?;

    let args = Args::parse();
    let venues = if args.venues.is_empty() {
        Venue::ALL.to_vec()
    } else {
        args.venues.clone()
    };

    println!(
        "Pomiar: para {}/{}, {} próbek, strumień {} s\n",
        args.base, args.quote, args.samples, args.feed_secs
    );

    let reports = join_all(venues.iter().map(|&venue| measure(venue, &args))).await;
    for (venue, report) in venues.iter().zip(reports) {
        match report {
            Ok(report) => print_report(*venue, &report, args.feed_secs),
            Err(e) => println!("{}\n  BŁĄD: {e:#}\n", venue.name()),
        }
    }
    println!(
        "* Szacunek: czas odbioru − znacznik czasu giełdy, skorygowany o różnicę zegarów.\n  \
         Dokładność ± asymetria trasy w obie strony."
    );
    Ok(())
}

async fn measure(venue: Venue, args: &Args) -> Result<Report> {
    let symbol = venue.symbol(&args.base, &args.quote);
    let rest = measure_rest(venue, args.samples).await.context("REST")?;

    let clock_offset_ms = rest.clock_offset_ms.unwrap_or(0.0);
    let ws = measure_ws(
        venue,
        &symbol,
        args.samples,
        args.feed_secs,
        clock_offset_ms,
    )
    .await
    .context("WebSocket")?;
    Ok(Report { symbol, rest, ws })
}

struct RestReport {
    cold_ms: f64,
    rtt: Summary,
    clock_offset_ms: Option<f64>,
}

async fn measure_rest(venue: Venue, samples: u32) -> Result<RestReport> {
    let client = reqwest::Client::builder().tcp_nodelay(true).build()?;

    let started = Instant::now();
    fetch_server_ms(&client, venue).await?;
    let cold_ms = elapsed_ms(started);

    let mut rtts = Vec::with_capacity(samples as usize);

    let mut best: Option<(f64, f64)> = None;
    for _ in 0..samples {
        sleep(REST_INTERVAL).await;
        let sent_ms = now_ms();
        let started = Instant::now();
        let server_ms = fetch_server_ms(&client, venue).await?;
        let rtt = elapsed_ms(started);
        rtts.push(rtt);
        if let Some(server_ms) = server_ms
            && best.is_none_or(|(best_rtt, _)| rtt < best_rtt)
        {
            best = Some((rtt, server_ms - (sent_ms + rtt / 2.0)));
        }
    }

    Ok(RestReport {
        cold_ms,
        rtt: Summary::from_samples(rtts).context("brak próbek")?,
        clock_offset_ms: best.map(|(_, offset)| offset),
    })
}

async fn fetch_server_ms(client: &reqwest::Client, venue: Venue) -> Result<Option<f64>> {
    let body = client
        .get(venue.rest_probe_url())
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    venue.parse_server_time_ms(&body)
}

struct WsReport {
    connect_ms: f64,
    rtt: Summary,
    book_updates: usize,
    feed_lag: Option<Summary>,
}

async fn measure_ws(
    venue: Venue,
    symbol: &str,
    samples: u32,
    feed_secs: u64,
    clock_offset_ms: f64,
) -> Result<WsReport> {
    let started = Instant::now();
    let (mut ws, _) = connect_async(venue.ws_public_url()).await?;
    let connect_ms = elapsed_ms(started);

    ws.send(Message::Text(venue.subscribe_msg(symbol).into()))
        .await?;

    let mut rtts = Vec::with_capacity(samples as usize);
    for id in 0..samples {
        let started = Instant::now();
        ws.send(ping_message(venue, id)).await?;
        loop {
            let msg = timeout(WS_TIMEOUT, ws.next())
                .await
                .context("brak odpowiedzi na ping")?
                .context("połączenie zamknięte przez giełdę")??;
            if let Message::Text(text) = &msg {
                venue.parse_feed(text)?;
            }
            if is_pong(venue, &msg) {
                break;
            }
        }
        rtts.push(elapsed_ms(started));
        sleep(WS_PING_INTERVAL).await;
    }

    let mut book_updates = 0;
    let mut lags = Vec::new();
    let mut last_exchange_ms: Option<f64> = None;
    let feed_end = sleep(Duration::from_secs(feed_secs));
    tokio::pin!(feed_end);
    let mut keepalive = interval(WS_KEEPALIVE);
    keepalive.tick().await;
    loop {
        tokio::select! {
            _ = &mut feed_end => break,
            _ = keepalive.tick() => ws.send(ping_message(venue, 0)).await?,
            msg = ws.next() => {
                let msg = msg.context("połączenie zamknięte przez giełdę")??;
                let recv_ms = now_ms();
                let Message::Text(text) = msg else { continue };
                let Feed::Book(exchange_ms) = venue.parse_feed(&text)? else { continue };
                book_updates += 1;
                let Some(exchange_ms) = exchange_ms else { continue };

                if last_exchange_ms.is_some_and(|last| exchange_ms > last) {
                    lags.push(recv_ms - (exchange_ms - clock_offset_ms));
                }
                last_exchange_ms = Some(last_exchange_ms.map_or(exchange_ms, |l| l.max(exchange_ms)));
            }
        }
    }
    let _ = ws.close(None).await;

    Ok(WsReport {
        connect_ms,
        rtt: Summary::from_samples(rtts).context("brak próbek")?,
        book_updates,
        feed_lag: Summary::from_samples(lags),
    })
}

fn ping_message(venue: Venue, id: u32) -> Message {
    match venue.ping(id) {
        Ping::Text(text) => Message::Text(text.into()),
        Ping::Frame => Message::Ping(Default::default()),
    }
}

fn is_pong(venue: Venue, msg: &Message) -> bool {
    match msg {
        Message::Text(text) => venue.is_pong(text.as_str()),
        Message::Pong(_) => matches!(venue.ping(0), Ping::Frame),
        _ => false,
    }
}

fn print_report(venue: Venue, r: &Report, feed_secs: u64) {
    println!("{} — {}", venue.name(), r.symbol);
    println!(
        "  REST   pierwsze połączenie (DNS+TCP+TLS): {:7.1} ms",
        r.rest.cold_ms
    );
    println!("  REST   RTT        {}", r.rest.rtt);
    match r.rest.clock_offset_ms {
        Some(offset) => println!("  Zegar  giełda − lokalny: {offset:+.1} ms"),
        None => println!("  Zegar  giełda podaje czas w sekundach — przyjęto zegar lokalny (NTP)"),
    }
    println!("  WS     połączenie: {:7.1} ms", r.ws.connect_ms);
    println!("  WS     ping RTT   {}", r.ws.rtt);
    let rate = r.ws.book_updates as f64 / feed_secs as f64;
    match (&r.ws.feed_lag, r.ws.book_updates) {
        (_, 0) => println!("  Książka brak zmian w {feed_secs} s"),
        (Some(lag), _) => println!("  Książka {rate:.1} zmian/s, opóźnienie* {lag}"),
        (None, _) => println!("  Książka {rate:.1} zmian/s, giełda nie podaje znacznika czasu"),
    }
    println!();
}

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("zegar systemowy przed 1970")
        .as_secs_f64()
        * 1000.0
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}
