use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::selector::utc_date;

pub struct Ledger {
    dir: PathBuf,
    date: String,
    file: Option<File>,
}

impl Ledger {
    pub fn new(dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&dir).with_context(|| format!("tworzenie {}", dir.display()))?;
        Ok(Ledger {
            dir,
            date: String::new(),
            file: None,
        })
    }

    pub fn write(&mut self, mut line: Value) -> Result<()> {
        let now = now_ms();
        line["ts_ms"] = Value::from(now as u64);
        let date = utc_date(now);
        if self.file.is_none() || date != self.date {
            let path = self.dir.join(format!("{date}.jsonl"));
            self.file = Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .with_context(|| format!("zapis do {}", path.display()))?,
            );
            self.date = date;
        }
        if let Some(file) = &mut self.file {
            writeln!(file, "{line}")?;
        }
        Ok(())
    }

    pub fn today(&self) -> Vec<Value> {
        let path = self.dir.join(format!("{}.jsonl", utc_date(now_ms())));
        fs::read_to_string(path)
            .map(|text| {
                text.lines()
                    .filter_map(|l| serde_json::from_str(l).ok())
                    .collect()
            })
            .unwrap_or_default()
    }
}

pub fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}
