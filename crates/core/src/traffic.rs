//! Traffic counters reported by the engine on a node.

use serde::{Deserialize, Serialize};

/// Cumulative octet counters of a node, as the engine exports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrafficCounters {
    pub from_client: u64,
    pub to_client: u64,
}

/// Bytes since the previous reading of a cumulative counter.
///
/// A counter that went down was reset (the engine restarted) and counts from zero again, so
/// the current value is what passed since the reset.
pub fn traffic_delta(prev: u64, cur: u64) -> u64 {
    if cur >= prev { cur - prev } else { cur }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traffic_delta_counts_growth() {
        assert_eq!(traffic_delta(1_000, 1_500), 500);
        assert_eq!(traffic_delta(7, 7), 0);
    }

    #[test]
    fn traffic_delta_handles_counter_reset() {
        assert_eq!(traffic_delta(1_000, 50), 50);
        assert_eq!(traffic_delta(u64::MAX, 0), 0);
    }

    #[test]
    fn traffic_delta_does_not_overflow() {
        assert_eq!(traffic_delta(0, u64::MAX), u64::MAX);
    }
}
