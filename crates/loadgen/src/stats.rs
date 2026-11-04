//! Latency samples and the figures made of them.

use std::time::Duration;

use serde::Serialize;

/// The `p`-th percentile (nearest rank) of `sorted`, in the unit of the samples. `None` for no
/// samples.
pub fn percentile(sorted: &[u64], p: f64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    Some(sorted[rank.clamp(1, sorted.len()) - 1])
}

/// The figures of one kind of request.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    pub count: u64,
    pub errors: u64,
    pub p50_ms: Option<f64>,
    pub p99_ms: Option<f64>,
    pub max_ms: Option<f64>,
}

/// Samples of one kind of request.
#[derive(Debug, Default)]
pub struct Series {
    micros: Vec<u64>,
    errors: u64,
}

impl Series {
    pub fn ok(&mut self, took: Duration) {
        self.micros
            .push(u64::try_from(took.as_micros()).unwrap_or(u64::MAX));
    }

    pub fn failed(&mut self) {
        self.errors += 1;
    }

    pub fn summary(&self) -> Summary {
        let mut sorted = self.micros.clone();
        sorted.sort_unstable();
        let ms = |p: f64| percentile(&sorted, p).map(|us| us as f64 / 1000.0);
        Summary {
            count: sorted.len() as u64,
            errors: self.errors,
            p50_ms: ms(50.0),
            p99_ms: ms(99.0),
            max_ms: ms(100.0),
        }
    }

    pub fn merged(parts: &[&Series]) -> Series {
        let mut all = Series::default();
        for part in parts {
            all.micros.extend_from_slice(&part.micros);
            all.errors += part.errors;
        }
        all
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_uses_nearest_rank() {
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&v, 50.0), Some(50));
        assert_eq!(percentile(&v, 99.0), Some(99));
        assert_eq!(percentile(&v, 100.0), Some(100));
        assert_eq!(percentile(&v, 0.0), Some(1));
    }

    #[test]
    fn percentile_of_nothing_is_none() {
        assert_eq!(percentile(&[], 99.0), None);
    }

    #[test]
    fn percentile_of_one_sample_is_that_sample() {
        assert_eq!(percentile(&[7], 99.0), Some(7));
    }

    #[test]
    fn summary_counts_and_converts_to_milliseconds() {
        let mut s = Series::default();
        for ms in [10, 20, 30, 40] {
            s.ok(Duration::from_millis(ms));
        }
        s.failed();
        let sum = s.summary();
        assert_eq!(sum.count, 4);
        assert_eq!(sum.errors, 1);
        assert_eq!(sum.p50_ms, Some(20.0));
        assert_eq!(sum.max_ms, Some(40.0));
    }

    #[test]
    fn empty_summary_has_no_figures() {
        let sum = Series::default().summary();
        assert_eq!((sum.count, sum.errors), (0, 0));
        assert_eq!(sum.p99_ms, None);
    }

    #[test]
    fn merged_series_joins_samples_and_errors() {
        let mut a = Series::default();
        let mut b = Series::default();
        a.ok(Duration::from_millis(1));
        b.ok(Duration::from_millis(9));
        b.failed();
        let m = Series::merged(&[&a, &b]).summary();
        assert_eq!((m.count, m.errors), (2, 1));
        assert_eq!(m.max_ms, Some(9.0));
    }
}
