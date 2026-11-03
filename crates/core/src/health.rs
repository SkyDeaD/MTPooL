//! Node health: the conditions a node must meet to be eligible as a master.

use crate::settings::Settings;
use crate::types::Ts;

/// Hysteresis over a noisy probe: trips after enough failures in a row, recovers after
/// enough successes in a row. Thresholds are passed in so E2E can shrink them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Latch {
    pub up: bool,
    pub fails: u8,
    pub oks: u8,
}

impl Latch {
    /// A new latch is down: a node proves itself before it counts as healthy.
    pub fn new() -> Latch {
        Latch {
            up: false,
            fails: 0,
            oks: 0,
        }
    }

    /// Records one probe result. Returns `Some(new_state)` when the latch flips.
    pub fn observe(&mut self, ok: bool, fail_threshold: u32, ok_threshold: u32) -> Option<bool> {
        if ok {
            self.fails = 0;
            if self.up {
                self.oks = 0;
                return None;
            }
            self.oks = self.oks.saturating_add(1);
            if u32::from(self.oks) >= ok_threshold {
                self.up = true;
                self.oks = 0;
                return Some(true);
            }
        } else {
            self.oks = 0;
            if !self.up {
                self.fails = 0;
                return None;
            }
            self.fails = self.fails.saturating_add(1);
            if u32::from(self.fails) >= fail_threshold {
                self.up = false;
                self.fails = 0;
                return Some(false);
            }
        }
        None
    }
}

impl Default for Latch {
    fn default() -> Latch {
        Latch::new()
    }
}

/// Result of the latest Globalping check, as confirmed by the registrar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpStatus {
    Unknown,
    Ok {
        at: Ts,
    },
    Failed {
        at: Ts,
    },
    /// Globalping answered 429: no data, neither success nor failure. Only follows a
    /// success (see [`GpStatus::on_no_data`]); `Unknown` and `Failed` are never replaced by it.
    NoData {
        at: Ts,
    },
}

impl GpStatus {
    /// Status after an agent report with no data. A success stays acceptable (as `NoData`,
    /// until the report ages out into `ReportStale`); `Unknown` and `Failed` are unchanged,
    /// so a 429 cannot turn a blocked or unproven node into a healthy one.
    pub fn on_no_data(self, at: Ts) -> GpStatus {
        match self {
            GpStatus::Ok { .. } | GpStatus::NoData { .. } => GpStatus::NoData { at },
            GpStatus::Unknown | GpStatus::Failed { .. } => self,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeHealth {
    pub tcp: Latch,
    pub metrics: Latch,
    pub gp: GpStatus,
    pub last_heartbeat: Ts,
    /// Latest report with a measurement the registrar verified. A report with no data does
    /// not refresh it, which is why a long run of 429s ages out into `ReportStale`.
    pub last_report: Ts,
    /// The node's port matches the pool setting.
    pub port_ok: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailedCondition {
    Tcp,
    Metrics,
    Globalping,
    Port,
    HeartbeatStale,
    ReportStale,
}

/// Every Healthy condition that does not hold at `now`. Empty means the node may be a master.
pub fn failed_conditions(h: &NodeHealth, now: Ts, s: &Settings) -> Vec<FailedCondition> {
    let mut failed = Vec::new();
    if !h.tcp.up {
        failed.push(FailedCondition::Tcp);
    }
    if !h.metrics.up {
        failed.push(FailedCondition::Metrics);
    }
    if matches!(h.gp, GpStatus::Unknown | GpStatus::Failed { .. }) {
        failed.push(FailedCondition::Globalping);
    }
    if !h.port_ok {
        failed.push(FailedCondition::Port);
    }
    if !younger_than(h.last_heartbeat, now, s.heartbeat_ttl_sec) {
        failed.push(FailedCondition::HeartbeatStale);
    }
    if !younger_than(h.last_report, now, s.report_max_age_sec) {
        failed.push(FailedCondition::ReportStale);
    }
    failed
}

/// True when `at` is strictly younger than `max_age_sec`. A timestamp ahead of `now` counts
/// as fresh.
fn younger_than(at: Ts, now: Ts, max_age_sec: u32) -> bool {
    now.0.saturating_sub(at.0) < i64::from(max_age_sec) * 1000
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: Ts = Ts(10_000_000);

    fn ago(sec: i64) -> Ts {
        Ts(NOW.0 - sec * 1000)
    }

    fn up() -> Latch {
        Latch {
            up: true,
            fails: 0,
            oks: 0,
        }
    }

    fn healthy() -> NodeHealth {
        NodeHealth {
            tcp: up(),
            metrics: up(),
            gp: GpStatus::Ok { at: ago(30) },
            last_heartbeat: ago(5),
            last_report: ago(60),
            port_ok: true,
        }
    }

    #[test]
    fn latch_falls_after_3_rises_after_2() {
        let mut l = up();
        assert_eq!(l.observe(false, 3, 2), None);
        assert_eq!(l.observe(false, 3, 2), None);
        assert_eq!(l.observe(false, 3, 2), Some(false));
        assert!(!l.up);
        assert_eq!(l.observe(false, 3, 2), None, "already down: no new flip");

        assert_eq!(l.observe(true, 3, 2), None);
        assert_eq!(l.observe(true, 3, 2), Some(true));
        assert!(l.up);
    }

    #[test]
    fn latch_streak_must_be_unbroken() {
        let mut l = up();
        l.observe(false, 3, 2);
        l.observe(false, 3, 2);
        l.observe(true, 3, 2);
        assert_eq!(
            l.observe(false, 3, 2),
            None,
            "the ok reset the failure streak"
        );
        assert!(l.up);
    }

    #[test]
    fn new_latch_starts_down_and_follows_given_thresholds() {
        let mut l = Latch::new();
        assert!(!l.up);
        assert_eq!(l.observe(true, 1, 1), Some(true));
        assert_eq!(l.observe(false, 1, 1), Some(false));
    }

    #[test]
    fn all_ok_returns_empty() {
        assert!(failed_conditions(&healthy(), NOW, &Settings::default()).is_empty());
    }

    #[test]
    fn heartbeat_61s_old_fails() {
        let h = NodeHealth {
            last_heartbeat: ago(61),
            ..healthy()
        };
        assert_eq!(
            failed_conditions(&h, NOW, &Settings::default()),
            vec![FailedCondition::HeartbeatStale]
        );
        let h = NodeHealth {
            last_heartbeat: ago(59),
            ..healthy()
        };
        assert!(failed_conditions(&h, NOW, &Settings::default()).is_empty());
    }

    #[test]
    fn heartbeat_exactly_ttl_old_is_stale() {
        let h = NodeHealth {
            last_heartbeat: ago(60),
            ..healthy()
        };
        assert_eq!(
            failed_conditions(&h, NOW, &Settings::default()),
            vec![FailedCondition::HeartbeatStale],
            "healthy needs a heartbeat younger than the TTL"
        );
    }

    #[test]
    fn report_16min_old_fails() {
        let h = NodeHealth {
            last_report: ago(16 * 60),
            ..healthy()
        };
        assert_eq!(
            failed_conditions(&h, NOW, &Settings::default()),
            vec![FailedCondition::ReportStale]
        );
        let h = NodeHealth {
            last_report: ago(14 * 60),
            ..healthy()
        };
        assert!(failed_conditions(&h, NOW, &Settings::default()).is_empty());
    }

    #[test]
    fn gp_no_data_is_not_failure() {
        let h = NodeHealth {
            gp: GpStatus::NoData { at: ago(10) },
            ..healthy()
        };
        assert!(failed_conditions(&h, NOW, &Settings::default()).is_empty());

        let h = NodeHealth {
            gp: GpStatus::NoData { at: ago(10) },
            last_report: ago(16 * 60),
            ..healthy()
        };
        assert_eq!(
            failed_conditions(&h, NOW, &Settings::default()),
            vec![FailedCondition::ReportStale],
            "the stale report catches it, not Globalping"
        );
    }

    #[test]
    fn no_data_does_not_clear_failed_or_unknown() {
        let failed = GpStatus::Failed { at: ago(100) };
        assert_eq!(failed.on_no_data(ago(10)), failed);
        assert_eq!(GpStatus::Unknown.on_no_data(ago(10)), GpStatus::Unknown);

        let h = NodeHealth {
            gp: failed.on_no_data(ago(10)),
            ..healthy()
        };
        assert_eq!(
            failed_conditions(&h, NOW, &Settings::default()),
            vec![FailedCondition::Globalping]
        );

        assert_eq!(
            GpStatus::Ok { at: ago(100) }.on_no_data(ago(10)),
            GpStatus::NoData { at: ago(10) }
        );
        assert_eq!(
            GpStatus::NoData { at: ago(100) }.on_no_data(ago(10)),
            GpStatus::NoData { at: ago(10) }
        );
    }

    #[test]
    fn gp_unknown_and_failed_fail_globalping() {
        for gp in [GpStatus::Unknown, GpStatus::Failed { at: ago(10) }] {
            let h = NodeHealth { gp, ..healthy() };
            assert_eq!(
                failed_conditions(&h, NOW, &Settings::default()),
                vec![FailedCondition::Globalping]
            );
        }
    }

    #[test]
    fn each_condition_is_reported_in_order() {
        let h = NodeHealth {
            tcp: Latch::new(),
            metrics: Latch::new(),
            gp: GpStatus::Unknown,
            last_heartbeat: ago(120),
            last_report: ago(2000),
            port_ok: false,
        };
        assert_eq!(
            failed_conditions(&h, NOW, &Settings::default()),
            vec![
                FailedCondition::Tcp,
                FailedCondition::Metrics,
                FailedCondition::Globalping,
                FailedCondition::Port,
                FailedCondition::HeartbeatStale,
                FailedCondition::ReportStale,
            ]
        );
    }
}
