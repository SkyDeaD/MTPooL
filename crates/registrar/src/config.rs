//! Configuration from the environment. There is no config file.

use std::net::SocketAddr;
use std::path::PathBuf;

use reqwest::Url;

use crate::dns::cloudflare::DEFAULT_BASE as CLOUDFLARE_BASE;
use crate::sensors::check_host::DEFAULT_BASE as CHECK_HOST_BASE;
use crate::sensors::globalping::DEFAULT_BASE as GLOBALPING_BASE;
use crate::sensors::redflag::DEFAULT_BASE as REDFLAG_BASE;

pub const ENV_DB_PATH: &str = "MTPOOL_DB_PATH";
pub const ENV_LISTEN: &str = "MTPOOL_LISTEN";
pub const ENV_LOG: &str = "MTPOOL_LOG";

pub const ENV_GLOBALPING_URL: &str = "MTPOOL_GLOBALPING_URL";
pub const ENV_CHECK_HOST_URL: &str = "MTPOOL_CHECK_HOST_URL";
pub const ENV_REDFLAG_URL: &str = "MTPOOL_REDFLAG_URL";
pub const ENV_CLOUDFLARE_URL: &str = "MTPOOL_CLOUDFLARE_URL";
/// Lets nodes on private, loopback and shared addresses enroll. Only for a test stand.
pub const ENV_RELEASES_DIR: &str = "MTPOOL_RELEASES_DIR";
pub const ENV_ALLOW_PRIVATE_IPS: &str = "MTPOOL_ALLOW_PRIVATE_IPS";

pub const DEFAULT_DB_PATH: &str = "/var/lib/mtpool/registrar.db";
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8080";
pub const DEFAULT_LOG: &str = "info";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub db_path: PathBuf,
    /// Used when no socket is passed in through `listenfd`.
    pub listen: SocketAddr,
    /// `tracing` filter directives.
    pub log_filter: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("{var} must be a socket address like 127.0.0.1:8080, got {value:?}")]
    InvalidListen { var: &'static str, value: String },
    #[error("{var} must be an http or https URL, got {value:?}")]
    InvalidUrl { var: &'static str, value: String },
}

/// The roots of the external services. The defaults are the real ones; a test stand points them
/// at its own stand-ins through the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub globalping: Url,
    pub check_host: Url,
    pub redflag: Url,
    pub cloudflare: Url,
}

impl Endpoints {
    pub fn from_env() -> Result<Endpoints, ConfigError> {
        Endpoints::from_lookup(|name| std::env::var(name).ok())
    }

    /// `from_env` with the variables supplied by `get`. Empty values count as unset; a value that
    /// is not an http(s) URL is an error that names the variable.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Endpoints, ConfigError> {
        Ok(Endpoints {
            globalping: base(&get, ENV_GLOBALPING_URL, GLOBALPING_BASE)?,
            check_host: base(&get, ENV_CHECK_HOST_URL, CHECK_HOST_BASE)?,
            redflag: base(&get, ENV_REDFLAG_URL, REDFLAG_BASE)?,
            cloudflare: base(&get, ENV_CLOUDFLARE_URL, CLOUDFLARE_BASE)?,
        })
    }
}

/// The URL in `var`, or `default` when it is unset or empty.
pub fn base(
    get: &impl Fn(&str) -> Option<String>,
    var: &'static str,
    default: &str,
) -> Result<Url, ConfigError> {
    let value = get(var)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_owned());
    match Url::parse(&value) {
        Ok(url) if matches!(url.scheme(), "http" | "https") && url.host_str().is_some() => Ok(url),
        _ => Err(ConfigError::InvalidUrl { var, value }),
    }
}

/// Whether `MTPOOL_ALLOW_PRIVATE_IPS` is exactly `true`. Anything else, or nothing, is off.
pub fn allow_private_ips(get: impl Fn(&str) -> Option<String>) -> bool {
    get(ENV_ALLOW_PRIVATE_IPS).as_deref() == Some("true")
}

/// The directory `/releases/{name}` reads from: `MTPOOL_RELEASES_DIR` or the default.
pub fn releases_dir(get: impl Fn(&str) -> Option<String>) -> PathBuf {
    PathBuf::from(
        get(ENV_RELEASES_DIR)
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| crate::release::DEFAULT_RELEASES_DIR.to_owned()),
    )
}

impl Config {
    pub fn from_env() -> Result<Config, ConfigError> {
        Config::from_lookup(|name| std::env::var(name).ok())
    }

    /// `from_env` with the variables supplied by `get`. Empty values count as unset.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Config, ConfigError> {
        let get = |name: &str| get(name).filter(|v| !v.is_empty());
        let listen = get(ENV_LISTEN).unwrap_or_else(|| DEFAULT_LISTEN.to_owned());
        Ok(Config {
            db_path: PathBuf::from(get(ENV_DB_PATH).unwrap_or_else(|| DEFAULT_DB_PATH.to_owned())),
            listen: listen.parse().map_err(|_| ConfigError::InvalidListen {
                var: ENV_LISTEN,
                value: listen,
            })?,
            log_filter: get(ENV_LOG).unwrap_or_else(|| DEFAULT_LOG.to_owned()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn defaults_apply_when_nothing_is_set() {
        let c = Config::from_lookup(lookup(&[])).unwrap();
        assert_eq!(c.db_path, PathBuf::from("/var/lib/mtpool/registrar.db"));
        assert_eq!(c.listen, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(c.log_filter, "info");
    }

    #[test]
    fn variables_override_defaults() {
        let c = Config::from_lookup(lookup(&[
            ("MTPOOL_DB_PATH", "/tmp/x.db"),
            ("MTPOOL_LISTEN", "0.0.0.0:9000"),
            ("MTPOOL_LOG", "debug"),
        ]))
        .unwrap();
        assert_eq!(c.db_path, PathBuf::from("/tmp/x.db"));
        assert_eq!(c.listen, "0.0.0.0:9000".parse().unwrap());
        assert_eq!(c.log_filter, "debug");
    }

    #[test]
    fn bad_listen_address_is_an_error() {
        let err = Config::from_lookup(lookup(&[("MTPOOL_LISTEN", "nope")])).unwrap_err();
        assert!(err.to_string().contains("MTPOOL_LISTEN"), "{err}");
    }

    #[test]
    fn endpoints_default_to_the_real_services() {
        let e = Endpoints::from_lookup(lookup(&[])).unwrap();
        assert_eq!(e.globalping.as_str(), "https://api.globalping.io/");
        assert_eq!(e.check_host.as_str(), "https://check-host.net/");
        assert_eq!(e.redflag.as_str(), "https://api.yazaebalsyadelatproxy.cc/");
        assert_eq!(e.cloudflare.as_str(), "https://api.cloudflare.com/");
    }

    #[test]
    fn endpoints_can_be_replaced_one_by_one() {
        let e = Endpoints::from_lookup(lookup(&[
            (
                "MTPOOL_GLOBALPING_URL",
                "http://fakes.example.test:9000/globalping/",
            ),
            (
                "MTPOOL_CLOUDFLARE_URL",
                "http://fakes.example.test:9000/cloudflare",
            ),
        ]))
        .unwrap();
        assert_eq!(
            e.globalping.as_str(),
            "http://fakes.example.test:9000/globalping/"
        );
        assert_eq!(e.cloudflare.host_str(), Some("fakes.example.test"));
        assert_eq!(e.check_host.as_str(), "https://check-host.net/");
    }

    #[test]
    fn an_empty_endpoint_counts_as_unset() {
        let e = Endpoints::from_lookup(lookup(&[("MTPOOL_REDFLAG_URL", "")])).unwrap();
        assert_eq!(e.redflag.as_str(), "https://api.yazaebalsyadelatproxy.cc/");
    }

    #[test]
    fn a_bad_endpoint_names_its_variable() {
        for (var, value) in [
            ("MTPOOL_GLOBALPING_URL", "not a url"),
            ("MTPOOL_CHECK_HOST_URL", "ftp://example.test/"),
            ("MTPOOL_REDFLAG_URL", "/relative"),
            ("MTPOOL_CLOUDFLARE_URL", "http://"),
        ] {
            let err = Endpoints::from_lookup(lookup(&[(var, value)])).unwrap_err();
            assert!(err.to_string().contains(var), "{var}: {err}");
        }
    }

    #[test]
    fn the_releases_directory_has_a_default_and_can_be_replaced() {
        assert_eq!(
            releases_dir(lookup(&[])),
            PathBuf::from("/var/lib/mtpool/releases")
        );
        assert_eq!(
            releases_dir(lookup(&[("MTPOOL_RELEASES_DIR", "/srv/r")])),
            PathBuf::from("/srv/r")
        );
        assert_eq!(
            releases_dir(lookup(&[("MTPOOL_RELEASES_DIR", "")])),
            PathBuf::from("/var/lib/mtpool/releases")
        );
    }

    #[test]
    fn private_ips_are_off_unless_the_value_is_exactly_true() {
        assert!(!allow_private_ips(lookup(&[])));
        for value in ["", "1", "TRUE", "True", "yes", "true ", "false"] {
            assert!(
                !allow_private_ips(lookup(&[("MTPOOL_ALLOW_PRIVATE_IPS", value)])),
                "{value:?}"
            );
        }
        assert!(allow_private_ips(lookup(&[(
            "MTPOOL_ALLOW_PRIVATE_IPS",
            "true"
        )])));
    }
}
