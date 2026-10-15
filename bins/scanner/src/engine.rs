use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc::Receiver;
use tokio::time::interval;
use venues::{Quote, Venue};

use crate::clock::{now_ms, utc_date, utc_time};
use crate::feed::{Event, TopUpdate};
use crate::maker::MakerSim;
use crate::transit::Transit;
use crate::universe::{PairId, Universe};

const MAX_OWN_SPREAD: f64 = 0.10;
pub const MAX_PLAUSIBLE_GAP: f64 = 0.10;
const BEST_PER_STATUS: usize = 3;
const TRANSIT_TICK: Duration = Duration::from_millis(100);
const STALL: f64 = 3_000.0;

pub struct Config {
    pub min_net_bps: f64,
    pub data_dir: PathBuf,
    pub status_every: Duration,
    pub watch: Option<Watch>,
    pub maker: Option<crate::maker::Config>,
    pub transit: Option<crate::transit::Config>,
}

pub struct Watch {
    pub pairs: Vec<PairId>,
    pub every: Duration,
}

pub async fn run(mut rx: Receiver<Event>, universe: &Universe, mut cfg: Config) -> Result<()> {
    let maker = cfg.maker.take();
    let transit = cfg.transit.take();
    let mut engine = Engine::new(universe, &cfg, maker, transit)?;
    let mut transit_tick = interval(TRANSIT_TICK);
    let mut status = interval(cfg.status_every);
    status.tick().await;
    let mut view = cfg.watch.as_ref().map(|w| interval(w.every));
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        let view_tick = async {
            match &mut view {
                Some(view) => view.tick().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            event = rx.recv() => match event {
                Some(event) => engine.on_event(event)?,
                None => break,
            },
            _ = status.tick() => engine.status(cfg.status_every)?,
            _ = view_tick => engine.view(),
            _ = transit_tick.tick() => engine.transit_tick()?,
            _ = &mut shutdown => break,
        }
    }
    engine.close_all("shutdown")?;
    println!("Zatrzymano. Trwające okazje zapisane.");
    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut term) => tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        },
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[derive(Clone, Copy)]
pub struct Top {
    pub bid: f64,
    pub bid_qty: f64,
    pub ask: f64,
    pub ask_qty: f64,
}

impl Top {
    pub const UNKNOWN: Top = Top {
        bid: f64::NAN,
        bid_qty: f64::NAN,
        ask: f64::NAN,
        ask_qty: f64::NAN,
    };

    pub fn from_quote(q: &Quote) -> Self {
        Top {
            bid: q.bid,
            bid_qty: q.bid_qty.unwrap_or(f64::NAN),
            ask: q.ask,
            ask_qty: q.ask_qty.unwrap_or(f64::NAN),
        }
    }

    fn apply(&mut self, u: &TopUpdate) {
        if let Some(x) = u.bid {
            self.bid = x;
        }
        if let Some(x) = u.bid_qty {
            self.bid_qty = x;
        }
        if let Some(x) = u.ask {
            self.ask = x;
        }
        if let Some(x) = u.ask_qty {
            self.ask_qty = x;
        }
    }

    pub fn mid(&self) -> f64 {
        (self.bid + self.ask) / 2.0
    }

    pub fn is_usable(&self) -> bool {
        self.bid > 0.0
            && self.ask >= self.bid
            && (self.ask - self.bid) / self.ask <= MAX_OWN_SPREAD
            && self.bid_qty != 0.0
            && self.ask_qty != 0.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Route {
    pub pair: PairId,
    pub buy: Venue,
    pub sell: Venue,
}

#[derive(Clone, Copy)]
pub struct Signal {
    pub net_bps: f64,
    gross_bps: f64,
    pub size: Option<f64>,
}

impl Signal {
    pub fn of(route: Route, buy: &Top, sell: &Top) -> Option<Self> {
        if !buy.is_usable() || !sell.is_usable() {
            return None;
        }
        let gross = sell.bid / buy.ask - 1.0;
        if gross.abs() > MAX_PLAUSIBLE_GAP {
            return None;
        }
        let net = sell.bid * (1.0 - route.sell.taker_fee())
            / (buy.ask * (1.0 + route.buy.taker_fee()))
            - 1.0;
        let size = known(buy.ask_qty)
            .zip(known(sell.bid_qty))
            .map(|(ask_qty, bid_qty)| (ask_qty * buy.ask).min(bid_qty * sell.bid));
        if size.is_some_and(|s| s <= 0.0) {
            return None;
        }
        Some(Signal {
            net_bps: net * 1e4,
            gross_bps: gross * 1e4,
            size,
        })
    }
}

struct Episode {
    start_ms: f64,
    started: Instant,
    first: Signal,
    best: Signal,
    updates: u32,
}

impl Episode {
    fn open(signal: Signal) -> Self {
        Episode {
            start_ms: now_ms(),
            started: Instant::now(),
            first: signal,
            best: signal,
            updates: 1,
        }
    }

    fn observe(&mut self, signal: Signal) {
        self.updates += 1;
        if signal.net_bps > self.best.net_bps {
            self.best = signal;
        }
    }
}

#[derive(Default)]
struct Window {
    updates: [u64; Venue::COUNT],
    trades: [u64; Venue::COUNT],
    closed: u32,
    best: Vec<(Route, f64, f64)>,
}

struct Engine<'a> {
    universe: &'a Universe,
    min_net_bps: f64,
    tops: Vec<[Top; Venue::COUNT]>,
    live: [bool; Venue::COUNT],
    open: HashMap<Route, Episode>,
    log: JsonlLog,
    window: Window,
    ticks: Option<JsonlLog>,
    watched: Vec<PairId>,
    ticks_since_view: Vec<u64>,
    maker: Option<MakerSim<'a>>,
    transit: Option<Transit<'a>>,
    last_tick_ms: f64,
}

impl<'a> Engine<'a> {
    fn new(
        universe: &'a Universe,
        cfg: &Config,
        maker: Option<crate::maker::Config>,
        transit: Option<crate::transit::Config>,
    ) -> Result<Self> {
        let transit = transit
            .map(|t| Transit::new(universe, t, &cfg.data_dir))
            .transpose()?;
        let maker = maker
            .map(|m| MakerSim::new(universe, m, &cfg.data_dir))
            .transpose()?;
        let ticks = match cfg.watch {
            Some(_) => Some(JsonlLog::new(cfg.data_dir.join("ticks"))?),
            None => None,
        };
        Ok(Engine {
            universe,
            min_net_bps: cfg.min_net_bps,
            tops: vec![[Top::UNKNOWN; Venue::COUNT]; universe.pairs.len()],
            live: [false; Venue::COUNT],
            open: HashMap::new(),
            log: JsonlLog::new(cfg.data_dir.join("opportunities"))?,
            window: Window::default(),
            ticks,
            watched: cfg
                .watch
                .as_ref()
                .map_or_else(Vec::new, |w| w.pairs.clone()),
            ticks_since_view: vec![0; universe.pairs.len()],
            maker,
            transit,
            last_tick_ms: now_ms(),
        })
    }

    fn on_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Up(venue) => {
                self.live[venue.index()] = true;
                println!("{}: na żywo", venue.name());
            }
            Event::Down(venue) => {
                self.live[venue.index()] = false;
                for tops in &mut self.tops {
                    tops[venue.index()] = Top::UNKNOWN;
                }
                let routes: Vec<Route> = self
                    .open
                    .keys()
                    .filter(|r| r.buy == venue || r.sell == venue)
                    .copied()
                    .collect();
                for route in routes {
                    self.close(route, "venue_down")?;
                }
                if let Some(maker) = &mut self.maker {
                    maker.venue_down(venue);
                }
            }
            Event::Top(update) => {
                let (pair, venue) = (update.pair, update.venue);
                self.window.updates[venue.index()] += 1;
                self.tops[pair][venue.index()].apply(&update);
                if let Some(ticks) = &mut self.ticks {
                    self.ticks_since_view[pair] += 1;
                    let top = &self.tops[pair][venue.index()];
                    let now = now_ms();
                    ticks.write(
                        now,
                        &json!({
                            "ts_ms": round2(now),
                            "pair": self.universe.pairs[pair].to_string(),
                            "venue": venue.id(),
                            "bid": top.bid,
                            "bid_qty": top.bid_qty,
                            "ask": top.ask,
                            "ask_qty": top.ask_qty,
                        }),
                    )?;
                }

                if self.live[venue.index()] {
                    for other in Venue::ALL {
                        if other != venue && self.live[other.index()] {
                            self.check(Route {
                                pair,
                                buy: venue,
                                sell: other,
                            })?;
                            self.check(Route {
                                pair,
                                buy: other,
                                sell: venue,
                            })?;
                        }
                    }
                    if let Some(maker) = &mut self.maker {
                        maker.on_top(pair, venue, &self.tops[pair], &self.live);
                    }
                }
            }
            Event::Trade(trade) => {
                self.window.trades[trade.venue.index()] += 1;
                if self.live[trade.venue.index()]
                    && let Some(maker) = &mut self.maker
                {
                    maker.on_trade(&trade, &self.live)?;
                }
            }
        }
        Ok(())
    }

    fn check(&mut self, route: Route) -> Result<()> {
        let tops = &self.tops[route.pair];
        let signal = Signal::of(route, &tops[route.buy.index()], &tops[route.sell.index()])
            .filter(|s| s.net_bps > self.min_net_bps);
        match (signal, self.open.get_mut(&route)) {
            (Some(signal), Some(episode)) => episode.observe(signal),
            (Some(signal), None) => {
                self.open.insert(route, Episode::open(signal));
                if let Some(transit) = &mut self.transit {
                    transit.on_open(route, &tops[route.buy.index()], &signal);
                }
            }
            (None, Some(_)) => self.close(route, "closed")?,
            (None, None) => {}
        }
        Ok(())
    }

    fn close(&mut self, route: Route, reason: &str) -> Result<()> {
        let Some(episode) = self.open.remove(&route) else {
            return Ok(());
        };
        let duration_ms = episode.started.elapsed().as_secs_f64() * 1000.0;
        self.window.closed += 1;
        self.window
            .best
            .push((route, episode.best.net_bps, duration_ms));
        self.window.best.sort_by(|a, b| b.1.total_cmp(&a.1));
        self.window.best.truncate(BEST_PER_STATUS);
        let line = json!({
            "type": "episode",
            "pair": self.universe.pairs[route.pair].to_string(),
            "buy": route.buy.id(),
            "sell": route.sell.id(),
            "start_ms": episode.start_ms as u64,
            "duration_ms": round2(duration_ms),
            "first_net_bps": round2(episode.first.net_bps),
            "first_gross_bps": round2(episode.first.gross_bps),
            "first_size": episode.first.size.map(round2),
            "best_net_bps": round2(episode.best.net_bps),
            "best_size": episode.best.size.map(round2),
            "updates": episode.updates,
            "end": reason,
        });
        self.log.write(episode.start_ms, &line)
    }

    fn transit_tick(&mut self) -> Result<()> {
        let now = now_ms();
        let stalled = now - self.last_tick_ms > STALL;
        self.last_tick_ms = now;
        if stalled {
            self.reset_after_stall()?;
        }
        if let Some(maker) = &mut self.maker {
            maker.tick(&self.tops)?;
        }
        let Some(transit) = &mut self.transit else {
            return Ok(());
        };
        let open = &self.open;
        transit.tick(&self.tops, |route| open.contains_key(route))
    }

    fn reset_after_stall(&mut self) -> Result<()> {
        println!(
            "[{} UTC] przerwa w pracy (uśpienie?): reset cen i symulacji",
            utc_time(now_ms())
        );
        self.tops.fill([Top::UNKNOWN; Venue::COUNT]);
        self.close_all("stall")?;
        if let Some(maker) = &mut self.maker {
            maker.reset();
        }
        if let Some(transit) = &mut self.transit {
            transit.reset();
        }
        Ok(())
    }

    fn view(&mut self) {
        println!("[{} UTC] obserwowane pary:", utc_time(now_ms()));
        for &pair in &self.watched {
            let ticks = std::mem::take(&mut self.ticks_since_view[pair]);
            let best = match best_route(pair, &self.tops[pair], |v| self.live[v.index()]) {
                Some((route, signal)) => format!(
                    "{:<9} → {:<9} {:+8.1} bp  oferta {:>6}",
                    route.buy.name(),
                    route.sell.name(),
                    signal.net_bps,
                    signal.size.map_or("?".to_owned(), |s| format!("{s:.0}"))
                ),
                None => "brak notowań na 2 giełdach".to_owned(),
            };
            println!(
                "  {:<12} {best}  | ticki: {ticks}",
                self.universe.pairs[pair].to_string()
            );
        }
    }

    fn close_all(&mut self, reason: &str) -> Result<()> {
        let routes: Vec<Route> = self.open.keys().copied().collect();
        for route in routes {
            self.close(route, reason)?;
        }
        Ok(())
    }

    fn status(&mut self, every: Duration) -> Result<()> {
        let now = now_ms();
        let counts: Vec<String> = Venue::ALL
            .iter()
            .map(|v| {
                let state = if self.live[v.index()] {
                    ""
                } else {
                    " (offline)"
                };
                format!("{} {}{state}", v.name(), self.window.updates[v.index()])
            })
            .collect();
        println!(
            "[{} UTC] zmiany w {} s: {} | okazje: {} trwa, {} zakończonych",
            utc_time(now),
            every.as_secs(),
            counts.join(", "),
            self.open.len(),
            self.window.closed
        );
        if let Some(maker) = &mut self.maker {
            let trades: u64 = self.window.trades.iter().sum();
            println!(
                "    maker (symulacja): {} wykonań, zysk {:+.2}, bez zabezpieczenia {} | transakcji na rynku: {trades}",
                maker.window.fills, maker.window.profit, maker.window.unhedged
            );
            maker.window = Default::default();
        }
        for (route, net_bps, duration_ms) in &self.window.best {
            println!(
                "    {} {} → {}: {net_bps:+.1} bp przez {duration_ms:.0} ms",
                self.universe.pairs[route.pair],
                route.buy.name(),
                route.sell.name()
            );
        }

        let live: Vec<&str> = Venue::ALL
            .into_iter()
            .filter(|v| self.live[v.index()])
            .map(Venue::id)
            .collect();
        let updates: Map<String, Value> = Venue::ALL
            .iter()
            .map(|v| (v.id().to_owned(), json!(self.window.updates[v.index()])))
            .collect();
        let line = json!({
            "type": "status",
            "ts_ms": now as u64,
            "interval_secs": every.as_secs(),
            "live": live,
            "updates": updates,
            "open": self.open.len(),
        });

        if live.len() >= 2 {
            if let Some(maker) = &mut self.maker {
                maker.log_status(every)?;
            }
            if let Some(transit) = &mut self.transit {
                transit.log_status(every)?;
            }
        }
        self.window = Window::default();
        self.log.write(now, &line)
    }
}

pub fn best_route(
    pair: PairId,
    tops: &[Top; Venue::COUNT],
    usable: impl Fn(Venue) -> bool,
) -> Option<(Route, Signal)> {
    let mut best: Option<(Route, Signal)> = None;
    for buy in Venue::ALL {
        for sell in Venue::ALL {
            if buy == sell || !usable(buy) || !usable(sell) {
                continue;
            }
            let route = Route { pair, buy, sell };
            if let Some(signal) = Signal::of(route, &tops[buy.index()], &tops[sell.index()])
                && best.is_none_or(|(_, b)| signal.net_bps > b.net_bps)
            {
                best = Some((route, signal));
            }
        }
    }
    best
}

fn known(x: f64) -> Option<f64> {
    x.is_finite().then_some(x)
}

pub fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

pub struct JsonlLog {
    dir: PathBuf,
    date: String,
    file: Option<File>,
}

impl JsonlLog {
    pub fn new(dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&dir).with_context(|| format!("tworzenie {}", dir.display()))?;
        Ok(JsonlLog {
            dir,
            date: String::new(),
            file: None,
        })
    }

    pub fn write(&mut self, ts_ms: f64, line: &Value) -> Result<()> {
        let date = utc_date(ts_ms);
        if self.file.is_none() || date != self.date {
            let path = self.dir.join(format!("{date}.jsonl"));
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("zapis do {}", path.display()))?;
            self.file = Some(file);
            self.date = date;
        }
        if let Some(file) = &mut self.file {
            writeln!(file, "{line}")?;
        }
        Ok(())
    }
}
