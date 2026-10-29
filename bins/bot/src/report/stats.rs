use chrono::{DateTime, Datelike, Duration as Days, NaiveDate, Utc, Weekday};

use super::HOUR_MS;

pub(super) fn mean(xs: impl Iterator<Item = f64>) -> f64 {
    let (sum, n) = xs.fold((0.0, 0usize), |(s, n), x| (s + x, n + 1));
    if n == 0 { 0.0 } else { sum / n as f64 }
}

pub(super) fn pct(xs: &mut [f64], p: f64) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.sort_by(f64::total_cmp);
    let rank = ((p / 100.0) * xs.len() as f64).ceil().max(1.0) as usize;
    xs[rank.min(xs.len()) - 1]
}

fn pl_offset_hours(ms: f64) -> i64 {
    let Some(t) = DateTime::from_timestamp_millis(ms as i64) else {
        return 1;
    };
    let last_sunday = |month: u32| -> Option<DateTime<Utc>> {
        let first_next = if month == 12 {
            NaiveDate::from_ymd_opt(t.year() + 1, 1, 1)?
        } else {
            NaiveDate::from_ymd_opt(t.year(), month + 1, 1)?
        };
        let mut day = first_next - Days::days(1);
        while day.weekday() != Weekday::Sun {
            day -= Days::days(1);
        }
        Some(day.and_hms_opt(1, 0, 0)?.and_utc())
    };
    match (last_sunday(3), last_sunday(10)) {
        (Some(start), Some(end)) if t >= start && t < end => 2,
        _ => 1,
    }
}

pub fn pl_time(ms: f64, format: &str) -> String {
    let shifted = ms + pl_offset_hours(ms) as f64 * HOUR_MS;
    DateTime::from_timestamp_millis(shifted as i64)
        .map(|t| t.format(format).to_string())
        .unwrap_or_default()
}

pub(super) fn pl_hour(ms: f64) -> i64 {
    ((ms / HOUR_MS).floor() as i64 + pl_offset_hours(ms)).rem_euclid(24)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn polish_time_follows_summer_time() {
        assert_eq!(pl_hour(1_791_784_800_000.0), 8);

        assert_eq!(pl_hour(1_796_104_800_000.0), 7);
    }

    #[test]
    fn percentiles_by_nearest_rank() {
        let mut xs = [40.0, 10.0, 30.0, 20.0];
        assert_eq!(pct(&mut xs, 50.0), 20.0);
        assert_eq!(pct(&mut xs, 90.0), 40.0);
        assert_eq!(pct(&mut [], 50.0), 0.0);
    }
}
