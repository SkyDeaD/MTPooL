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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_lets_4_in_and_slides() {
        let limiter = Limiter::new(4, Duration::from_secs(60));
        let t0 = Instant::now();
        for second in 0..4 {
            assert!(
                limiter.try_acquire(t0 + Duration::from_secs(second)),
                "place {second}"
            );
        }
        // A refusal takes nothing and does not move the window.
        assert!(!limiter.try_acquire(t0 + Duration::from_secs(30)));
        assert!(!limiter.try_acquire(t0 + Duration::from_secs(59)));
        // The first place frees up 60 seconds after it was taken, not earlier.
        assert!(limiter.try_acquire(t0 + Duration::from_secs(60)));
        assert!(!limiter.try_acquire(t0 + Duration::from_secs(60)));
        assert!(limiter.try_acquire(t0 + Duration::from_secs(61)));
        assert!(limiter.try_acquire(t0 + Duration::from_secs(62)));
        assert!(limiter.try_acquire(t0 + Duration::from_secs(63)));
        assert!(!limiter.try_acquire(t0 + Duration::from_secs(63)));
        // After a quiet minute everything is free again.
        assert!(limiter.try_acquire(t0 + Duration::from_secs(200)));
    }
}
