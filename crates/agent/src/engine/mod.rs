//! The proxy engine on the node. [`Engine`] is what the agent needs from it: its metrics, its
//! health and a way to apply the pool's users, domain and port. [`telemt::TelemtEngine`] is the
//! adapter for telemt, [`meko::MekoEngine`] runs it on a MEKO host, [`mtproxyl::MtProxyLEngine`]
//! drives MTProxyL through its command line, and [`detect::detect_engine`] says which of them
//! the host has.

use async_trait::async_trait;
use mtpool_protocol::agent::{AgentConfig, Dc2Report, MetricsReport};

pub mod dc2;
pub mod detect;
pub mod http;
pub mod meko;
pub mod mtproxyl;
pub mod prom;
pub mod telemt;

pub use mtpool_protocol::public::Engine as EngineKind;

/// What can go wrong when the agent talks to the engine.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The engine did not answer, or answered with something that is not a success.
    #[error("the engine's HTTP interface failed: {0}")]
    Http(String),
    /// The engine's configuration cannot be read or does not hold what the agent needs.
    #[error("the engine configuration is not usable: {0}")]
    Config(String),
    /// A file could not be read or written.
    #[error("{what}: {source}")]
    Io {
        what: String,
        source: std::io::Error,
    },
    /// A command on the host (`systemctl`) failed.
    #[error("{0}")]
    Host(String),
    /// The new configuration failed and the old one could not be brought back either.
    #[error("the new configuration failed ({reason}) and rolling back failed too: {cause}")]
    RollbackFailed { reason: String, cause: String },
}

pub type Result<T> = std::result::Result<T, EngineError>;

/// What the pool wants the engine to serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredConfig {
    /// `(name, secret_hex)` of the users the pool manages.
    pub users: Vec<(String, String)>,
    /// The TLS domain the proxy pretends to be.
    pub sni: String,
    pub port: u16,
}

impl From<&AgentConfig> for DesiredConfig {
    fn from(config: &AgentConfig) -> DesiredConfig {
        DesiredConfig {
            users: config
                .users
                .iter()
                .map(|user| (user.name.clone(), user.secret_hex.clone()))
                .collect(),
            sni: config.sni.clone(),
            port: config.port,
        }
    }
}

/// How the engine is doing. `ok` with a `reason` is a warning: the engine works, but something is
/// lost (`user_telemetry_disabled`: no client or traffic statistics).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineHealth {
    pub ok: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// The engine already served exactly this: nothing was touched, nothing restarted.
    Unchanged,
    /// The configuration was written and the restarted engine answered.
    Applied,
    /// The restarted engine did not come up; the previous configuration is back. The text says
    /// why.
    RolledBack(String),
}

#[async_trait]
pub trait Engine: Send + Sync {
    fn kind(&self) -> EngineKind;

    /// The port the engine listens on, from its own configuration.
    fn port(&self) -> Option<u16>;

    /// Totals and clients from the engine's metrics. An engine that does not answer is an `Err`;
    /// one that answers but is not serving is `ok: false`.
    async fn metrics(&self) -> Result<MetricsReport>;

    /// The check of the second data center. `None` when the engine has no interface for it.
    async fn dc2(&self) -> Option<Dc2Report>;

    async fn health(&self) -> EngineHealth;

    /// Makes the engine serve `desired`, restarting it only if something changed.
    async fn apply_config(&self, desired: &DesiredConfig) -> Result<ApplyOutcome>;
}
