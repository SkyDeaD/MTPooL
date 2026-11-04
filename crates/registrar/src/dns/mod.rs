//! DNS: the provider that holds the records and the worker that answers `Effect::PushDns`.
//!
//! `core` decides what a domain should resolve to and what to do about a failure (backoff and
//! events). The worker here only does the calls and reports the outcome as `Command::DnsDone`.

use std::time::Duration;

use async_trait::async_trait;
use mtpool_core::{LiveRecord, NewRecord};

pub mod cloudflare;
pub mod worker;

/// The comment every record the pool creates carries. A record with exactly this comment is the
/// pool's own and may be deleted; any other record is never touched. The marker lives at the
/// provider, so it survives a lost database and shows in the Cloudflare panel.
pub const MANAGED_COMMENT: &str = "MTPooL";

/// A record as the provider holds it, and whether the pool created it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRecord {
    pub live: LiveRecord,
    pub managed: bool,
}

/// Why a call to the provider gave nothing. Never carries the token.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DnsError {
    /// No token is set, so nothing was sent.
    #[error("no API token is set")]
    NotConfigured,
    /// The token is revoked, invalid or has no right to the zone.
    #[error("the token is refused")]
    Auth,
    #[error("no zone holds the domain")]
    ZoneNotFound,
    /// Still limited after waiting once.
    #[error("rate limited, retry after {retry_after:?}")]
    RateLimited { retry_after: Duration },
    #[error("the provider did not answer in time")]
    Timeout,
    #[error("the provider answered with status {0}")]
    Http(u16),
    #[error("the provider is not reachable: {0}")]
    Transport(String),
    #[error("an answer that cannot be used: {0}")]
    Unexpected(String),
}

impl DnsError {
    /// The short word `DnsDone` carries and the event shows.
    pub fn reason(&self) -> String {
        match self {
            DnsError::NotConfigured => "not_configured".to_owned(),
            DnsError::Auth => "auth".to_owned(),
            DnsError::ZoneNotFound => "zone_not_found".to_owned(),
            DnsError::RateLimited { .. } => "rate_limited".to_owned(),
            DnsError::Timeout => "timeout".to_owned(),
            DnsError::Http(status) => format!("http_{status}"),
            DnsError::Transport(_) => "transport".to_owned(),
            DnsError::Unexpected(_) => "unexpected".to_owned(),
        }
    }
}

/// A DNS service. Every call is one step the worker may stop after.
#[async_trait]
pub trait DnsProvider: Send + Sync {
    /// The id of the zone that holds `domain`; `None` when no zone does.
    async fn zone_id(&self, domain: &str) -> Result<Option<String>, DnsError>;

    /// Every record named `name`, of any kind.
    async fn list(&self, zone: &str, name: &str) -> Result<Vec<ProviderRecord>, DnsError>;

    /// Creates a record marked as the pool's own and returns its id.
    async fn create(
        &self,
        zone: &str,
        record: NewRecord,
        ttl: u32,
        proxied: bool,
    ) -> Result<String, DnsError>;

    /// Deletes a record. One that is already gone is not an error.
    async fn delete(&self, zone: &str, id: &str) -> Result<(), DnsError>;
}
