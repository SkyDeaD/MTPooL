//! The local configuration, `/etc/mtpool/agent.toml`.

use std::path::{Path, PathBuf};

use mtpool_protocol::agent::Intervals;
use reqwest::Url;

pub const DEFAULT_CONFIG_PATH: &str = "/etc/mtpool/agent.toml";
pub const DEFAULT_STATE_DIR: &str = "/var/lib/mtpool";

/// The port check-host checks unless the configuration says otherwise: SSH.
pub const DEFAULT_CHECK_HOST_PORT: u16 = 22;

/// Intervals until the registrar's `/agent/config` says otherwise.
pub const DEFAULT_INTERVALS: Intervals = Intervals {
    heartbeat_sec: 15,
    globalping_sec: 300,
    metrics_sec: 60,
    sync_sec: 60,
};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{var} is not valid: {message}")]
    Environment { var: &'static str, message: String },
    #[error("cannot read the configuration {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("the configuration {path} is not valid: {message}")]
    Invalid { path: PathBuf, message: String },
}

/// Overrides of the default intervals. Each applies until `/agent/config` answers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IntervalOverrides {
    pub heartbeat_sec: Option<u32>,
    pub globalping_sec: Option<u32>,
    pub metrics_sec: Option<u32>,
    pub sync_sec: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalConfig {
    /// Base URL of the registrar, `https://...` (plain `http` only for a loopback host).
    pub registrar: Url,
    /// The name the node enrolls under.
    pub name: String,
    pub state_dir: PathBuf,
    /// The port of the proxy. When absent the engine adapter reports it.
    pub port: Option<u16>,
    pub antiscan: bool,
    /// The port check-host opens from Russia to tell whether the address is blocked; 0 switches
    /// the check off.
    pub check_host_port: u16,
    pub intervals: IntervalOverrides,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    registrar: String,
    name: String,
    state_dir: Option<PathBuf>,
    port: Option<i64>,
    antiscan: Option<bool>,
    check_host_port: Option<i64>,
    intervals: Option<RawIntervals>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIntervals {
    heartbeat_sec: Option<u32>,
    globalping_sec: Option<u32>,
    metrics_sec: Option<u32>,
    sync_sec: Option<u32>,
}

impl LocalConfig {
    pub fn load(path: &Path) -> Result<LocalConfig, ConfigError> {
        LocalConfig::load_allowing(path, false)
    }

    /// [`LocalConfig::load`]; with `allow_insecure_registrar` the registrar may be plain `http`
    /// on any host (a test network).
    pub fn load_allowing(
        path: &Path,
        allow_insecure_registrar: bool,
    ) -> Result<LocalConfig, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        LocalConfig::parse_allowing(&text, path, allow_insecure_registrar)
    }

    /// Parses `text`; `origin` names the file in error messages.
    pub fn parse(text: &str, origin: &Path) -> Result<LocalConfig, ConfigError> {
        LocalConfig::parse_allowing(text, origin, false)
    }

    /// [`LocalConfig::parse`] with the rule about plain `http` lifted when
    /// `allow_insecure_registrar` is set.
    pub fn parse_allowing(
        text: &str,
        origin: &Path,
        allow_insecure_registrar: bool,
    ) -> Result<LocalConfig, ConfigError> {
        let invalid = |message: String| ConfigError::Invalid {
            path: origin.to_owned(),
            message,
        };
        let raw: Raw = toml_edit::de::from_str(text).map_err(|e| invalid(e.to_string()))?;

        let registrar = parse_registrar(&raw.registrar, allow_insecure_registrar)
            .map_err(|m| invalid(format!("registrar: {m}")))?;
        if !valid_name(&raw.name) {
            return Err(invalid(
                "name must be 1 to 32 letters, digits and `._-`, starting and ending with a letter or digit"
                    .to_owned(),
            ));
        }
        let port = match raw.port {
            None => None,
            Some(p) => Some(
                u16::try_from(p)
                    .ok()
                    .filter(|p| *p != 0)
                    .ok_or_else(|| invalid("port must be between 1 and 65535".to_owned()))?,
            ),
        };
        let check_host_port = match raw.check_host_port {
            None => DEFAULT_CHECK_HOST_PORT,
            Some(p) => u16::try_from(p)
                .map_err(|_| invalid("check_host_port must be between 0 and 65535".to_owned()))?,
        };
        let raw_intervals = raw.intervals.unwrap_or(RawIntervals {
            heartbeat_sec: None,
            globalping_sec: None,
            metrics_sec: None,
            sync_sec: None,
        });
        for (key, value) in [
            ("heartbeat_sec", raw_intervals.heartbeat_sec),
            ("globalping_sec", raw_intervals.globalping_sec),
            ("metrics_sec", raw_intervals.metrics_sec),
            ("sync_sec", raw_intervals.sync_sec),
        ] {
            if value == Some(0) {
                return Err(invalid(format!(
                    "intervals.{key} must be at least 1 second"
                )));
            }
        }
        Ok(LocalConfig {
            registrar,
            name: raw.name,
            state_dir: raw
                .state_dir
                .unwrap_or_else(|| PathBuf::from(DEFAULT_STATE_DIR)),
            port,
            antiscan: raw.antiscan.unwrap_or(true),
            check_host_port,
            intervals: IntervalOverrides {
                heartbeat_sec: raw_intervals.heartbeat_sec,
                globalping_sec: raw_intervals.globalping_sec,
                metrics_sec: raw_intervals.metrics_sec,
                sync_sec: raw_intervals.sync_sec,
            },
        })
    }

    /// The intervals to use before the registrar has answered.
    pub fn intervals(&self) -> Intervals {
        Intervals {
            heartbeat_sec: self
                .intervals
                .heartbeat_sec
                .unwrap_or(DEFAULT_INTERVALS.heartbeat_sec),
            globalping_sec: self
                .intervals
                .globalping_sec
                .unwrap_or(DEFAULT_INTERVALS.globalping_sec),
            metrics_sec: self
                .intervals
                .metrics_sec
                .unwrap_or(DEFAULT_INTERVALS.metrics_sec),
            sync_sec: self
                .intervals
                .sync_sec
                .unwrap_or(DEFAULT_INTERVALS.sync_sec),
        }
    }
}

/// The registrar address. Tokens travel in it, so it is `https`; plain `http` is for a registrar
/// on the same machine (development and tests), or on any host when `allow_insecure` is set.
fn parse_registrar(value: &str, allow_insecure: bool) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|e| format!("not a URL ({e})"))?;
    let Some(host) = url.host_str() else {
        return Err("the address has no host".to_owned());
    };
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    match url.scheme() {
        "https" => {}
        "http" if loopback || allow_insecure => {}
        _ => {
            return Err(
                "the address must start with https:// (http:// only for localhost)".to_owned(),
            );
        }
    }
    Ok(url)
}

/// The rule of the registrar for the name of a node: 1 to 32 letters, digits and `._-`, starting
/// and ending with a letter or digit.
fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=32).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

pub const ENV_GLOBALPING_URL: &str = "MTPOOL_GLOBALPING_URL";
pub const ENV_CHECK_HOST_URL: &str = "MTPOOL_CHECK_HOST_URL";
pub const ENV_IP_SOURCES: &str = "MTPOOL_IP_SOURCES";
pub const ENV_IPSUM_URL: &str = "MTPOOL_IPSUM_URL";
/// Lets the node run on a private test network. Only for a test stand.
pub const ENV_ALLOW_PRIVATE_IPS: &str = "MTPOOL_ALLOW_PRIVATE_IPS";

/// What the environment changes in the binary: the external services it talks to (a test stand
/// points them at stand-ins) and the private-network switch. The defaults are the real ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overrides {
    pub globalping_url: String,
    pub check_host_url: String,
    /// Two to eight URLs of plain-text address services.
    pub ip_sources: Vec<String>,
    pub ipsum_url: String,
    pub allow_private_ips: bool,
}

impl Overrides {
    pub fn from_env() -> Result<Overrides, ConfigError> {
        Overrides::from_lookup(|name| std::env::var(name).ok())
    }

    /// `from_env` with the variables supplied by `get`. Empty values count as unset. Every error
    /// names the variable.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Overrides, ConfigError> {
        let get = |name: &str| get(name).filter(|v| !v.is_empty());
        let url = |var: &'static str, default: &str| -> Result<String, ConfigError> {
            let value = get(var).unwrap_or_else(|| default.to_owned());
            check_url(var, &value)?;
            Ok(value)
        };
        let ip_sources = match get(ENV_IP_SOURCES) {
            None => crate::ip_detect::IP_SOURCES.map(String::from).to_vec(),
            Some(list) => {
                let sources: Vec<String> =
                    list.split(',').map(|item| item.trim().to_owned()).collect();
                if !(2..=8).contains(&sources.len()) {
                    return Err(ConfigError::Environment {
                        var: ENV_IP_SOURCES,
                        message: format!("needs 2 to 8 URLs, got {}", sources.len()),
                    });
                }
                for source in &sources {
                    check_url(ENV_IP_SOURCES, source)?;
                }
                sources
            }
        };
        Ok(Overrides {
            globalping_url: url(ENV_GLOBALPING_URL, crate::gp::DEFAULT_BASE)?,
            check_host_url: url(ENV_CHECK_HOST_URL, crate::check_host::DEFAULT_BASE)?,
            ip_sources,
            ipsum_url: url(ENV_IPSUM_URL, crate::antiscan::IPSUM_URL)?,
            allow_private_ips: get(ENV_ALLOW_PRIVATE_IPS).as_deref() == Some("true"),
        })
    }
}

fn check_url(var: &'static str, value: &str) -> Result<(), ConfigError> {
    match Url::parse(value) {
        Ok(url) if matches!(url.scheme(), "http" | "https") && url.host_str().is_some() => Ok(()),
        _ => Err(ConfigError::Environment {
            var,
            message: format!("{value:?} is not an http or https URL"),
        }),
    }
}
