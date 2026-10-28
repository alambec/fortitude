use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use hdrhistogram::Histogram;
use serde::Serialize;

/// Latency histogram in microseconds, from 1µs to 10 minutes.
pub fn histogram() -> Histogram<u64> {
    Histogram::new_with_bounds(1, 600_000_000, 3).expect("valid histogram bounds")
}

pub fn record(histogram: &mut Histogram<u64>, elapsed: Duration) {
    histogram.saturating_record(elapsed.as_micros().try_into().unwrap_or(u64::MAX));
}

#[derive(Debug, Serialize)]
pub struct Latency {
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

impl From<&Histogram<u64>> for Latency {
    fn from(h: &Histogram<u64>) -> Self {
        let ms = |us: u64| us as f64 / 1000.0;
        Self {
            p50_ms: ms(h.value_at_quantile(0.50)),
            p90_ms: ms(h.value_at_quantile(0.90)),
            p99_ms: ms(h.value_at_quantile(0.99)),
            max_ms: ms(h.max()),
        }
    }
}

/// Print the result as one JSON line and append it to `out` when given.
pub fn emit(result: &impl Serialize, out: Option<&Path>) -> anyhow::Result<()> {
    let line = serde_json::to_string(result)?;
    println!("{line}");
    if let Some(out) = out {
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(out)?;
        writeln!(file, "{line}")?;
    }
    Ok(())
}
