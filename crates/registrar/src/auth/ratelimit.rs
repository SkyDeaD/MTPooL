//! Limit of failed token attempts per client address. In memory: a restart clears it.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};

use mtpool_core::Ts;

/// Failed attempts allowed per address in one window.
pub const MAX_FAILURES: u32 = 10;
/// Length of the window, counted from the first failure.
pub const WINDOW_MS: i64 = 10 * 60 * 1000;
/// Addresses tracked at most. Past this (after pruning expired windows) the table is reset
/// rather than growing without bound.
const MAX_ENTRIES: usize = 100_000;
const PRUNE_EVERY_MS: i64 = 60 * 1000;

#[derive(Debug, Clone, Copy)]
struct Entry {
    window_start: Ts,
    failures: u32,
}

/// The key an address is counted under: an IPv6 client usually holds a whole /64, so the
/// prefix is the key; an IPv4-mapped address counts as the IPv4 one.
fn key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let masked = u128::from(v6) & !((1u128 << 64) - 1);
                IpAddr::V6(Ipv6Addr::from(masked))
            }
        },
    }
}

#[derive(Debug, Default)]
pub struct RateLimiter {
    entries: HashMap<IpAddr, Entry>,
    last_prune: Option<Ts>,
}

impl RateLimiter {
    /// `Some(seconds)` until the address may try again, when it is blocked.
    pub fn blocked(&self, ip: IpAddr, now: Ts) -> Option<u64> {
        let e = self.entries.get(&key(ip))?;
        let end = e.window_start.0.saturating_add(WINDOW_MS);
        if e.failures >= MAX_FAILURES && now.0 < end {
            let ms = end - now.0;
            Some(u64::try_from((ms + 999) / 1000).unwrap_or(1).max(1))
        } else {
            None
        }
    }

    pub fn record_failure(&mut self, ip: IpAddr, now: Ts) {
        self.prune(now);
        let e = self.entries.entry(key(ip)).or_insert(Entry {
            window_start: now,
            failures: 0,
        });
        if now.0 >= e.window_start.0.saturating_add(WINDOW_MS) {
            *e = Entry {
                window_start: now,
                failures: 0,
            };
        }
        e.failures = e.failures.saturating_add(1);
    }

    fn prune(&mut self, now: Ts) {
        let due = self
            .last_prune
            .is_none_or(|t| now.0 - t.0 >= PRUNE_EVERY_MS || now.0 < t.0);
        if !due && self.entries.len() < MAX_ENTRIES {
            return;
        }
        self.last_prune = Some(now);
        self.entries
            .retain(|_, e| now.0 < e.window_start.0.saturating_add(WINDOW_MS));
        if self.entries.len() >= MAX_ENTRIES {
            self.entries.clear();
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn blocks_on_the_tenth_failure_and_reports_the_wait() {
        let mut l = RateLimiter::default();
        let a = ip("203.0.113.1");
        for _ in 0..MAX_FAILURES - 1 {
            l.record_failure(a, Ts(1_000));
            assert_eq!(l.blocked(a, Ts(1_000)), None);
        }
        l.record_failure(a, Ts(1_000));
        assert_eq!(l.blocked(a, Ts(1_000)), Some(600));
        assert_eq!(l.blocked(a, Ts(1_000 + 599_001)), Some(1));
        assert_eq!(l.blocked(ip("203.0.113.2"), Ts(1_000)), None);
    }

    #[test]
    fn window_ends_and_counting_restarts() {
        let mut l = RateLimiter::default();
        let a = ip("203.0.113.1");
        for _ in 0..MAX_FAILURES {
            l.record_failure(a, Ts(0));
        }
        assert!(l.blocked(a, Ts(WINDOW_MS - 1)).is_some());
        assert_eq!(l.blocked(a, Ts(WINDOW_MS)), None);
        l.record_failure(a, Ts(WINDOW_MS));
        assert_eq!(
            l.blocked(a, Ts(WINDOW_MS)),
            None,
            "a new window starts at one"
        );
    }

    #[test]
    fn expired_entries_are_pruned() {
        let mut l = RateLimiter::default();
        for i in 0..50u8 {
            l.record_failure(ip(&format!("198.51.100.{i}")), Ts(0));
        }
        assert_eq!(l.len(), 50);
        l.record_failure(ip("203.0.113.9"), Ts(WINDOW_MS + PRUNE_EVERY_MS));
        assert_eq!(l.len(), 1);
    }

    #[test]
    fn ipv6_counts_per_64_and_mapped_v4_as_v4() {
        let mut l = RateLimiter::default();
        for i in 0..MAX_FAILURES {
            l.record_failure(ip(&format!("2001:db8:1:2::{i:x}")), Ts(0));
        }
        assert!(l.blocked(ip("2001:db8:1:2:ffff::1"), Ts(0)).is_some());
        assert!(l.blocked(ip("2001:db8:1:3::1"), Ts(0)).is_none());
        for _ in 0..MAX_FAILURES {
            l.record_failure(ip("::ffff:192.0.2.1"), Ts(0));
        }
        assert!(l.blocked(ip("192.0.2.1"), Ts(0)).is_some());
    }
}
