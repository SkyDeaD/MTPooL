//! Configuration from the environment. There is no config file.

use std::net::SocketAddr;
use std::path::PathBuf;

pub const ENV_DB_PATH: &str = "MTPOOL_DB_PATH";
pub const ENV_LISTEN: &str = "MTPOOL_LISTEN";
pub const ENV_LOG: &str = "MTPOOL_LOG";

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
}
