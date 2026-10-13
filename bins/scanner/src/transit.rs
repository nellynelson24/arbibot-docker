use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{Map, Value, json};
use venues::Venue;

use crate::clock::now_ms;
use crate::engine::{JsonlLog, Route, Signal, Top, round2};
use crate::universe::Universe;

pub const TRANSFER_SECS: [u64; 6] = [5, 15, 30, 60, 120, 300];
const SAME_GAP: Duration = Duration::from_secs(10);

pub struct Config {
    pub min_net_bps: f64,
    pub reaction: Duration,
}

struct Probe {
    route: Route,
    started: Instant,
    start_ms: f64,
    entry_ask: f64,
    entry_net_bps: f64,
    entry_size: Option<f64>,
    alive: Option<bool>,
    sells: Vec<(f64, Option<f64>)>,
}

pub struct Transit<'a> {
    universe: &'a Universe,
    cfg: Config,
    probes: Vec<Probe>,
    log: JsonlLog,
}

impl<'a> Transit<'a> {
    pub fn new(universe: &'a Universe, cfg: Config, data_dir: &Path) -> Result<Self> {
        Ok(Transit {
            universe,
            cfg,
            probes: Vec::new(),
            log: JsonlLog::new(data_dir.join("transit"))?,
        })
    }

    pub fn log_status(&mut self, every: Duration) -> Result<()> {
        let now = now_ms();
        let line = json!({"type": "status", "ts_ms": now as u64, "interval_secs": every.as_secs()});
        self.log.write(now, &line)
    }

    pub fn reset(&mut self) {
        self.probes.clear();
    }

    pub fn on_open(&mut self, route: Route, buy: &Top, signal: &Signal) {
        if signal.net_bps < self.cfg.min_net_bps
            || self
                .probes
                .iter()
                .any(|p| p.route == route && p.started.elapsed() < SAME_GAP)
        {
            return;
        }
        self.probes.push(Probe {
            route,
            started: Instant::now(),
            start_ms: now_ms(),
            entry_ask: buy.ask,
            entry_net_bps: signal.net_bps,
            entry_size: buy.ask_qty.is_finite().then_some(buy.ask_qty * buy.ask),
            alive: None,
            sells: Vec::with_capacity(TRANSFER_SECS.len()),
        });
    }

    pub fn tick(
        &mut self,
        tops: &[[Top; Venue::COUNT]],
        is_open: impl Fn(&Route) -> bool,
    ) -> Result<()> {
        let now = Instant::now();
        for probe in &mut self.probes {
            let elapsed = now.duration_since(probe.started);
            if probe.alive.is_none() && elapsed >= self.cfg.reaction {
                probe.alive = Some(is_open(&probe.route));
            }
            while let Some(&secs) = TRANSFER_SECS.get(probe.sells.len())
                && elapsed >= Duration::from_secs(secs)
            {
                let sell = &tops[probe.route.pair][probe.route.sell.index()];
                let net_bps = if sell.is_usable() {
                    (sell.bid * (1.0 - probe.route.sell.taker_fee())
                        / (probe.entry_ask * (1.0 + probe.route.buy.taker_fee()))
                        - 1.0)
                        * 1e4
                } else {
                    f64::NAN
                };
                let size = sell.bid_qty.is_finite().then_some(sell.bid_qty * sell.bid);
                probe.sells.push((net_bps, size));
            }
        }

        let (done, pending): (Vec<Probe>, Vec<Probe>) = self
            .probes
            .drain(..)
            .partition(|p| p.sells.len() == TRANSFER_SECS.len());
        self.probes = pending;
        for probe in done {
            self.write(&probe)?;
        }
        Ok(())
    }

    fn write(&mut self, probe: &Probe) -> Result<()> {
        let sells: Map<String, Value> = TRANSFER_SECS
            .iter()
            .zip(&probe.sells)
            .map(|(secs, (net_bps, size))| {
                (
                    secs.to_string(),
                    json!({"net_bps": round2(*net_bps), "size": size.map(round2)}),
                )
            })
            .collect();
        let line = json!({
            "type": "transit",
            "pair": self.universe.pairs[probe.route.pair].to_string(),
            "buy": probe.route.buy.id(),
            "sell": probe.route.sell.id(),
            "start_ms": probe.start_ms as u64,
            "entry_net_bps": round2(probe.entry_net_bps),
            "entry_size": probe.entry_size.map(round2),
            "alive": probe.alive.unwrap_or(false),
            "sells": sells,
        });
        self.log.write(probe.start_ms, &line)
    }
}
