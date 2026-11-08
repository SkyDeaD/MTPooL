//! The limit on how often a sensor may call its service.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

/// A sliding window: at most `max` stamps inside `window`.
pub struct Limiter {
    max: usize,
    window: Duration,
    stamps: Mutex<VecDeque<Instant>>,
}

impl Limiter {
    pub fn new(max: usize, window: Duration) -> Limiter {
        Limiter {
            max,
            window,
            stamps: Mutex::new(VecDeque::new()),
        }
    }

    /// Takes a place in the window, or says there is none. A refusal takes nothing.
    pub fn try_acquire(&self, now: Instant) -> bool {
        let mut stamps = self.stamps.lock().unwrap_or_else(|e| e.into_inner());
        while let Some(&oldest) = stamps.front() {
            if oldest + self.window <= now {
                stamps.pop_front();
            } else {
                break;
            }
        }
        if stamps.len() < self.max {
            stamps.push_back(now);
            true
        } else {
            false
        }
    }
}
