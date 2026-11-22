//! Latency statistics for the glass-to-glass harness (§11).
//!
//! The pattern renderer and QR decoder arrive with M1; the statistics and
//! regression gate are here so per-stage `tracing` timings can be reported
//! from day one.

use std::time::Duration;

/// p50/p95/p99 summary of a set of samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub count: usize,
    pub p50: Duration,
    pub p95: Duration,
    pub p99: Duration,
    pub max: Duration,
}

/// Nearest-rank percentile summary. `None` for an empty input.
pub fn summarize(samples: &[Duration]) -> Option<Summary> {
    if samples.is_empty() {
        return None;
    }
    let mut v = samples.to_vec();
    v.sort_unstable();
    let pct = |p: f64| v[((p / 100.0 * v.len() as f64).ceil() as usize).clamp(1, v.len()) - 1];
    Some(Summary {
        count: v.len(),
        p50: pct(50.0),
        p95: pct(95.0),
        p99: pct(99.0),
        max: *v.last().expect("non-empty"),
    })
}

/// Release gate: a regression of more than 10% blocks the release.
pub fn is_regression(baseline: Duration, current: Duration) -> bool {
    current.as_secs_f64() > baseline.as_secs_f64() * 1.10
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles() {
        let s: Vec<_> = (1..=100).map(Duration::from_millis).collect();
        let sum = summarize(&s).unwrap();
        assert_eq!(sum.p50, Duration::from_millis(50));
        assert_eq!(sum.p95, Duration::from_millis(95));
        assert_eq!(sum.p99, Duration::from_millis(99));
        assert_eq!(sum.max, Duration::from_millis(100));
        assert!(summarize(&[]).is_none());
    }

    #[test]
    fn regression_gate() {
        let b = Duration::from_millis(40);
        assert!(!is_regression(b, Duration::from_millis(44)));
        assert!(is_regression(b, Duration::from_millis(45)));
    }
}
