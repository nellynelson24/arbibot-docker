use std::fmt;

pub struct Summary {
    pub n: usize,
    pub min: f64,
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
    pub max: f64,
}

impl Summary {
    pub fn from_samples(mut samples: Vec<f64>) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }
        samples.sort_by(f64::total_cmp);
        let pct = |p: f64| samples[((p * samples.len() as f64).ceil() as usize).saturating_sub(1)];
        Some(Self {
            n: samples.len(),
            min: samples[0],
            p50: pct(0.50),
            p90: pct(0.90),
            p99: pct(0.99),
            max: samples[samples.len() - 1],
        })
    }
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "min {:7.1}  p50 {:7.1}  p90 {:7.1}  p99 {:7.1}  max {:7.1} ms  (n={})",
            self.min, self.p50, self.p90, self.p99, self.max, self.n
        )
    }
}
