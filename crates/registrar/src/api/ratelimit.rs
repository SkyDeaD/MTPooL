//! Limit on requests to the public API: a fixed window per client address. In memory: a
//! restart clears it.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use mtpool_core::Ts;

use super::ApiError;
use crate::auth::middleware::ClientIp;
use crate::auth::ratelimit::key;
use crate::server::AppState;

/// Requests allowed per address in one window.
pub const MAX_REQUESTS: u32 = 120;
/// Length of the window, counted from the first request.
pub const WINDOW_MS: i64 = 60 * 1000;
/// Addresses tracked at most. Past this (after pruning expired windows) the table is reset
/// rather than growing without bound.
const MAX_ENTRIES: usize = 100_000;
const PRUNE_EVERY_MS: i64 = 60 * 1000;

#[derive(Debug, Clone, Copy)]
struct Entry {
    window_start: i64,
    count: u32,
}

#[derive(Debug, Default)]
struct Table {
    entries: HashMap<IpAddr, Entry>,
    last_prune: Option<i64>,
}

#[derive(Debug, Default)]
pub struct RequestLimiter {
    table: Mutex<Table>,
}

impl RequestLimiter {
    pub fn new() -> RequestLimiter {
        RequestLimiter::default()
    }

    /// Counts a request of `ip`. `Err(seconds)` is how long until its window ends, when the
    /// address is over the limit; a refused request is not counted.
    pub fn hit(&self, ip: IpAddr, now: Ts) -> Result<(), u64> {
        let now = now.0;
        let mut table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        table.prune(now);
        let entry = table.entries.entry(key(ip)).or_insert(Entry {
            window_start: now,
            count: 0,
        });
        // A clock that went back starts a new window instead of locking the address out.
        if now < entry.window_start || now - entry.window_start >= WINDOW_MS {
            *entry = Entry {
                window_start: now,
                count: 0,
            };
        }
        if entry.count >= MAX_REQUESTS {
            let left = entry.window_start + WINDOW_MS - now;
            return Err(u64::try_from((left + 999) / 1000).unwrap_or(1).max(1));
        }
        entry.count += 1;
        Ok(())
    }
}

impl Table {
    fn prune(&mut self, now: i64) {
        let due = self
            .last_prune
            .is_none_or(|last| now < last || now - last >= PRUNE_EVERY_MS);
        if !due && self.entries.len() < MAX_ENTRIES {
            return;
        }
        self.last_prune = Some(now);
        self.entries
            .retain(|_, e| now >= e.window_start && now - e.window_start < WINDOW_MS);
        if self.entries.len() >= MAX_ENTRIES {
            self.entries.clear();
        }
    }
}

/// Middleware of the public routes: 429 with `Retry-After` past the limit.
pub async fn limit(
    State(app): State<AppState>,
    ClientIp(ip): ClientIp,
    request: Request,
    next: Next,
) -> Response {
    match app.limiter.hit(ip, (app.clock)()) {
        Ok(()) => next.run(request).await,
        Err(wait) => {
            tracing::warn!(%ip, wait_seconds = wait, "public API request refused: over the limit");
            ApiError::rate_limited(wait).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn counts_per_window_and_address() {
        let l = RequestLimiter::new();
        for _ in 0..MAX_REQUESTS {
            assert!(l.hit(ip("192.0.2.1"), Ts(1_000)).is_ok());
        }
        assert_eq!(l.hit(ip("192.0.2.1"), Ts(1_000)), Err(60));
        assert_eq!(l.hit(ip("192.0.2.1"), Ts(31_000)), Err(30));
        assert_eq!(l.hit(ip("192.0.2.1"), Ts(60_999)), Err(1));
        assert!(l.hit(ip("192.0.2.2"), Ts(1_000)).is_ok(), "another address");
        assert!(l.hit(ip("192.0.2.1"), Ts(61_000)).is_ok(), "a new window");
    }

    #[test]
    fn an_ipv6_client_counts_by_its_prefix() {
        let l = RequestLimiter::new();
        for _ in 0..MAX_REQUESTS {
            assert!(l.hit(ip("2001:db8::1"), Ts(0)).is_ok());
        }
        assert!(l.hit(ip("2001:db8::ffff"), Ts(0)).is_err());
    }

    #[test]
    fn a_clock_that_went_back_does_not_lock_out() {
        let l = RequestLimiter::new();
        for _ in 0..MAX_REQUESTS {
            l.hit(ip("192.0.2.1"), Ts(500_000)).unwrap();
        }
        assert!(l.hit(ip("192.0.2.1"), Ts(1_000)).is_ok());
    }
}
