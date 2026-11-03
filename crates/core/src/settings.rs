//! Pool settings: defaults come from the spec, thresholds are settings so E2E can shrink them.

use std::collections::HashSet;
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    // Agent intervals.
    pub heartbeat_interval_sec: u32,
    pub globalping_interval_sec: u32,
    pub metrics_interval_sec: u32,
    pub sync_interval_sec: u32,

    // Healthy conditions.
    pub heartbeat_ttl_sec: u32,
    pub report_max_age_sec: u32,
    pub tcp_probe_interval_sec: u32,
    pub tcp_probe_timeout_sec: u32,
    /// Latch trips after this many failures in a row.
    pub latch_fail_threshold: u32,
    /// Latch recovers after this many successes in a row.
    pub latch_ok_threshold: u32,

    // Quarantine.
    pub dead_after_sec: u32,
    pub quarantine_max_sec: u32,
    pub round_interval_sec: u32,
    /// Interval used after `rounds_before_slow` rounds without a decision.
    pub round_interval_slow_sec: u32,
    pub rounds_before_slow: u32,
    pub clean_rounds_to_exit: u32,
    pub reverify_pause_sec: u32,

    // Masters and DNS.
    pub masters_per_domain: u32,
    /// 0 turns rotation off.
    pub rotation_ttl_sec: u32,
    pub dns_ttl: u32,
    pub dns_proxied: bool,

    // Proxy.
    pub tls_domain: String,
    pub proxy_port: u16,
    pub domains: Vec<String>,
    pub proxy_users: Vec<ProxyUser>,

    pub srmd: SrmdSettings,
    pub globalping: GlobalpingSettings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyUser {
    pub name: String,
    /// 32 hex characters.
    pub secret_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrmdSettings {
    pub enabled: bool,
    pub max_nodes_per_domain: u32,
    pub max_domains: u32,
    /// Ticks the divergence must hold before one action is taken.
    pub diverge_ticks: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GlobalpingSettings {
    pub probe_limit: u32,
    /// Fewer probes with a result than this means `Abstain`.
    pub min_probes: u32,
    /// Success share below this means `Blocked`.
    pub success_threshold: f64,
    /// Newest agent report usable as a round vote.
    pub vote_report_max_age_sec: u32,
    /// Accepted measurement age: this far into the past...
    pub window_past_sec: u32,
    /// ...and this far into the future.
    pub window_future_sec: u32,
    /// Below this many remaining credits the sensor abstains.
    pub min_limit_remaining: u32,
    pub allow_credits: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsError {
    Invalid {
        field: &'static str,
        reason: &'static str,
    },
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SettingsError::Invalid { field, reason } => write!(f, "{field}: {reason}"),
        }
    }
}

impl std::error::Error for SettingsError {}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            heartbeat_interval_sec: 15,
            globalping_interval_sec: 300,
            metrics_interval_sec: 60,
            sync_interval_sec: 60,
            heartbeat_ttl_sec: 60,
            report_max_age_sec: 900,
            tcp_probe_interval_sec: 5,
            tcp_probe_timeout_sec: 3,
            latch_fail_threshold: 3,
            latch_ok_threshold: 2,
            dead_after_sec: 600,
            quarantine_max_sec: 3600,
            round_interval_sec: 300,
            round_interval_slow_sec: 900,
            rounds_before_slow: 3,
            clean_rounds_to_exit: 2,
            reverify_pause_sec: 900,
            masters_per_domain: 3,
            rotation_ttl_sec: 1800,
            dns_ttl: 60,
            dns_proxied: false,
            tls_domain: String::new(),
            proxy_port: 443,
            domains: Vec::new(),
            proxy_users: Vec::new(),
            srmd: SrmdSettings::default(),
            globalping: GlobalpingSettings::default(),
        }
    }
}

impl Default for SrmdSettings {
    fn default() -> Self {
        SrmdSettings {
            enabled: false,
            max_nodes_per_domain: 5,
            max_domains: 50,
            diverge_ticks: 20,
        }
    }
}

impl Default for GlobalpingSettings {
    fn default() -> Self {
        GlobalpingSettings {
            probe_limit: 20,
            min_probes: 10,
            success_threshold: 0.5,
            vote_report_max_age_sec: 600,
            window_past_sec: 600,
            window_future_sec: 60,
            min_limit_remaining: 20,
            allow_credits: false,
        }
    }
}

fn check(ok: bool, field: &'static str, reason: &'static str) -> Result<(), SettingsError> {
    if ok {
        Ok(())
    } else {
        Err(SettingsError::Invalid { field, reason })
    }
}

fn positive(v: u32, field: &'static str) -> Result<(), SettingsError> {
    check(v > 0, field, "must be greater than 0")
}

impl Settings {
    pub fn validate(&self) -> Result<(), SettingsError> {
        for (v, field) in [
            (self.heartbeat_interval_sec, "heartbeat_interval_sec"),
            (self.globalping_interval_sec, "globalping_interval_sec"),
            (self.metrics_interval_sec, "metrics_interval_sec"),
            (self.sync_interval_sec, "sync_interval_sec"),
            (self.heartbeat_ttl_sec, "heartbeat_ttl_sec"),
            (self.report_max_age_sec, "report_max_age_sec"),
            (self.tcp_probe_interval_sec, "tcp_probe_interval_sec"),
            (self.tcp_probe_timeout_sec, "tcp_probe_timeout_sec"),
            (self.latch_fail_threshold, "latch_fail_threshold"),
            (self.latch_ok_threshold, "latch_ok_threshold"),
            (self.dead_after_sec, "dead_after_sec"),
            (self.quarantine_max_sec, "quarantine_max_sec"),
            (self.round_interval_sec, "round_interval_sec"),
            (self.round_interval_slow_sec, "round_interval_slow_sec"),
            (self.rounds_before_slow, "rounds_before_slow"),
            (self.clean_rounds_to_exit, "clean_rounds_to_exit"),
            (self.reverify_pause_sec, "reverify_pause_sec"),
            (self.dns_ttl, "dns_ttl"),
        ] {
            positive(v, field)?;
        }
        check(
            (1..=8).contains(&self.masters_per_domain),
            "masters_per_domain",
            "must be between 1 and 8",
        )?;
        check(
            self.rotation_ttl_sec <= 43_200,
            "rotation_ttl_sec",
            "must be at most 43200",
        )?;
        check(self.proxy_port != 0, "proxy_port", "must be greater than 0")?;
        check(
            self.domains.iter().all(|d| !d.is_empty()),
            "domains",
            "must not contain empty names",
        )?;
        check(
            unique(self.domains.iter().map(String::as_str)),
            "domains",
            "must not contain duplicates",
        )?;
        for user in &self.proxy_users {
            check(
                !user.name.is_empty(),
                "proxy_users",
                "user name must not be empty",
            )?;
            check(
                user.secret_hex.len() == 32
                    && user.secret_hex.bytes().all(|b| b.is_ascii_hexdigit()),
                "proxy_users",
                "secret must be 32 hex characters",
            )?;
        }
        check(
            unique(self.proxy_users.iter().map(|u| u.name.as_str())),
            "proxy_users",
            "user names must be unique",
        )?;
        self.srmd.validate()?;
        self.globalping.validate()
    }
}

fn unique<'a>(items: impl Iterator<Item = &'a str>) -> bool {
    let mut seen = HashSet::new();
    items.into_iter().all(|i| seen.insert(i))
}

impl SrmdSettings {
    fn validate(&self) -> Result<(), SettingsError> {
        positive(self.max_nodes_per_domain, "srmd.max_nodes_per_domain")?;
        check(
            (1..=50).contains(&self.max_domains),
            "srmd.max_domains",
            "must be between 1 and 50",
        )?;
        positive(self.diverge_ticks, "srmd.diverge_ticks")
    }
}

impl GlobalpingSettings {
    fn validate(&self) -> Result<(), SettingsError> {
        positive(self.probe_limit, "globalping.probe_limit")?;
        check(
            self.min_probes > 0 && self.min_probes <= self.probe_limit,
            "globalping.min_probes",
            "must be between 1 and probe_limit",
        )?;
        check(
            self.success_threshold > 0.0 && self.success_threshold <= 1.0,
            "globalping.success_threshold",
            "must be in (0, 1]",
        )?;
        positive(
            self.vote_report_max_age_sec,
            "globalping.vote_report_max_age_sec",
        )?;
        positive(self.window_past_sec, "globalping.window_past_sec")?;
        positive(self.window_future_sec, "globalping.window_future_sec")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let s = Settings::default();
        assert_eq!(s.rotation_ttl_sec, 1800);
        assert_eq!(s.heartbeat_interval_sec, 15);
        assert_eq!(s.globalping_interval_sec, 300);
        assert_eq!(s.metrics_interval_sec, 60);
        assert_eq!(s.sync_interval_sec, 60);
        assert_eq!(s.heartbeat_ttl_sec, 60);
        assert_eq!(s.report_max_age_sec, 900);
        assert_eq!(s.tcp_probe_interval_sec, 5);
        assert_eq!(s.tcp_probe_timeout_sec, 3);
        assert_eq!((s.latch_fail_threshold, s.latch_ok_threshold), (3, 2));
        assert_eq!(s.dead_after_sec, 600);
        assert_eq!(s.quarantine_max_sec, 3600);
        assert_eq!(
            (s.round_interval_sec, s.round_interval_slow_sec),
            (300, 900)
        );
        assert_eq!(s.rounds_before_slow, 3);
        assert_eq!(s.clean_rounds_to_exit, 2);
        assert_eq!(s.reverify_pause_sec, 900);
        assert_eq!(s.dns_ttl, 60);
        assert_eq!(s.proxy_port, 443);
        assert!(s.domains.is_empty() && s.proxy_users.is_empty());
        assert!(!s.srmd.enabled);
        assert_eq!(s.srmd.max_nodes_per_domain, 5);
        assert_eq!(s.srmd.max_domains, 50);
        assert_eq!(s.srmd.diverge_ticks, 20);
        assert_eq!(s.globalping.probe_limit, 20);
        assert_eq!(s.globalping.min_probes, 10);
        assert_eq!(s.globalping.success_threshold, 0.5);
        assert_eq!(s.globalping.vote_report_max_age_sec, 600);
        assert_eq!(
            (s.globalping.window_past_sec, s.globalping.window_future_sec),
            (600, 60)
        );
        assert_eq!(s.globalping.min_limit_remaining, 20);
        assert!(!s.globalping.allow_credits);
    }

    #[test]
    fn default_settings_are_valid() {
        assert_eq!(Settings::default().validate(), Ok(()));
    }

    #[test]
    fn masters_per_domain_range_is_1_to_8() {
        for n in [1, 8] {
            assert!(
                Settings {
                    masters_per_domain: n,
                    ..Default::default()
                }
                .validate()
                .is_ok()
            );
        }
        for n in [0, 9] {
            assert!(
                Settings {
                    masters_per_domain: n,
                    ..Default::default()
                }
                .validate()
                .is_err()
            );
        }
    }

    #[test]
    fn rotation_ttl_range_is_0_to_43200() {
        for n in [0, 43_200] {
            assert!(
                Settings {
                    rotation_ttl_sec: n,
                    ..Default::default()
                }
                .validate()
                .is_ok()
            );
        }
        assert!(
            Settings {
                rotation_ttl_sec: 43_201,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn error_names_the_field() {
        let err = Settings {
            masters_per_domain: 9,
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        assert!(err.to_string().contains("masters_per_domain"), "{err}");
    }

    #[test]
    fn zero_intervals_and_thresholds_are_rejected() {
        let cases: Vec<Settings> = vec![
            Settings {
                heartbeat_interval_sec: 0,
                ..Default::default()
            },
            Settings {
                heartbeat_ttl_sec: 0,
                ..Default::default()
            },
            Settings {
                tcp_probe_interval_sec: 0,
                ..Default::default()
            },
            Settings {
                tcp_probe_timeout_sec: 0,
                ..Default::default()
            },
            Settings {
                latch_fail_threshold: 0,
                ..Default::default()
            },
            Settings {
                latch_ok_threshold: 0,
                ..Default::default()
            },
            Settings {
                round_interval_sec: 0,
                ..Default::default()
            },
            Settings {
                clean_rounds_to_exit: 0,
                ..Default::default()
            },
            Settings {
                proxy_port: 0,
                ..Default::default()
            },
        ];
        for s in cases {
            assert!(s.validate().is_err(), "{s:?}");
        }
    }

    #[test]
    fn globalping_and_srmd_are_validated() {
        let mut s = Settings::default();
        s.globalping.success_threshold = 1.5;
        assert!(s.validate().is_err());
        s.globalping.success_threshold = f64::NAN;
        assert!(s.validate().is_err());
        let mut s = Settings::default();
        s.globalping.min_probes = 21;
        assert!(
            s.validate().is_err(),
            "min_probes above probe_limit can never be met"
        );
        let mut s = Settings::default();
        s.srmd.max_domains = 51;
        assert!(s.validate().is_err());
        s.srmd.max_domains = 0;
        assert!(s.validate().is_err());
        let mut s = Settings::default();
        s.srmd.max_nodes_per_domain = 0;
        assert!(s.validate().is_err());
    }

    #[test]
    fn proxy_users_need_valid_secrets_and_unique_names() {
        let user = |name: &str, secret: &str| ProxyUser {
            name: name.into(),
            secret_hex: secret.into(),
        };
        let good = "00112233445566778899aabbccddeeff";
        let ok = Settings {
            proxy_users: vec![user("u1", good), user("u2", good)],
            ..Default::default()
        };
        assert!(ok.validate().is_ok());
        for bad in [
            vec![user("", good)],
            vec![user("u1", "xyz")],
            vec![user("u1", &good[..30])],
            vec![user("u1", good), user("u1", good)],
        ] {
            assert!(
                Settings {
                    proxy_users: bad,
                    ..Default::default()
                }
                .validate()
                .is_err()
            );
        }
    }

    #[test]
    fn domains_must_be_non_empty_and_unique() {
        let d = |v: &[&str]| Settings {
            domains: v.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        assert!(d(&["a.example.com", "b.example.com"]).validate().is_ok());
        assert!(d(&[""]).validate().is_err());
        assert!(d(&["a.example.com", "a.example.com"]).validate().is_err());
    }
}
