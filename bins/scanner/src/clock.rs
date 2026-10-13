use std::time::{SystemTime, UNIX_EPOCH};

use chrono::DateTime;

pub fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("zegar systemowy przed 1970")
        .as_secs_f64()
        * 1000.0
}

pub fn utc_date(ms: f64) -> String {
    format_utc(ms, "%Y-%m-%d")
}

pub fn utc_time(ms: f64) -> String {
    format_utc(ms, "%H:%M:%S")
}

fn format_utc(ms: f64, pattern: &str) -> String {
    DateTime::from_timestamp_millis(ms as i64)
        .map(|t| t.format(pattern).to_string())
        .unwrap_or_default()
}
