//! The dead-kill rule: a node whose proxy has been bad for ten minutes in a row leaves the pool
//! by itself (`/agent/retire`) instead of waiting for the pool to notice.

use std::time::Duration;

use tokio::time::Instant;

use crate::lifecycle::Health;
use crate::local_health::LocalHealth;

/// How long the proxy must stay bad before the node retires.
pub const DEAD_AFTER: Duration = Duration::from_secs(10 * 60);

/// What the local health says at `now`: good, silent, or dead (silent for [`DEAD_AFTER`]).
pub fn verdict(now: Instant, health: &LocalHealth) -> Health {
    if !health.is_silent() {
        return Health::Good;
    }
    match health.bad_since() {
        Some(since) if now.saturating_duration_since(since) >= DEAD_AFTER => Health::Dead,
        _ => Health::Silent,
    }
}
