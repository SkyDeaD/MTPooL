//! The admin API under `/api/v1/admin`: owners, tokens, audit, events and domains. The settings
//! document is the core `Settings` and is described as an object.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

/// An owner of nodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Owner {
    pub id: i64,
    pub name: String,
    pub node_limit: i64,
    pub note: String,
    /// RFC 3339, UTC.
    pub created_at: String,
}

/// `POST /admin/owners`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewOwner {
    pub name: String,
    pub node_limit: i64,
    #[serde(default)]
    pub note: Option<String>,
}

/// `PATCH /admin/owners/{id}`: the fields that are present change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OwnerPatch {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub node_limit: Option<i64>,
    #[serde(default)]
    pub note: Option<String>,
}

/// A token as the admin sees it: never the value, never the hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TokenMeta {
    pub id: i64,
    /// `admin`, `enroll`, `node`, `bot` or `metrics`.
    pub kind: String,
    pub prefix: String,
    pub owner_id: Option<i64>,
    pub scope: String,
    /// RFC 3339, UTC.
    pub created_at: String,
    pub expires_at: Option<String>,
    pub revoked_at: Option<String>,
    pub last_used_at: Option<String>,
}

/// `POST /admin/tokens`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewToken {
    /// `enroll`, `bot` or `metrics`.
    pub kind: String,
    /// Required for `enroll` and `bot`.
    #[serde(default)]
    pub owner_id: Option<i64>,
    /// RFC 3339.
    #[serde(default)]
    pub expires_at: Option<String>,
    /// `pool:read` lets a bot token register pool-wide webhooks.
    #[serde(default)]
    pub scope: Option<String>,
}

/// The answer to `POST /admin/tokens`: the only time the value is shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct NewTokenCreated {
    pub id: i64,
    pub kind: String,
    pub prefix: String,
    pub owner_id: Option<i64>,
    pub scope: String,
    pub created_at: String,
    pub expires_at: Option<String>,
    /// Shown once; it cannot be read again.
    pub token: String,
}

/// An entry of the audit log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AuditEntry {
    pub id: i64,
    /// RFC 3339, UTC.
    pub at: String,
    /// `token:<id>` or `session`.
    pub actor: String,
    #[schema(example = "owner.create")]
    pub action: String,
    pub target: Option<String>,
    /// What changed. Never a secret: settings changes list field names only.
    #[schema(value_type = Object)]
    pub detail: Value,
}

/// An entry of the event log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct EventEntry {
    pub id: i64,
    /// RFC 3339, UTC.
    pub at: String,
    #[schema(example = "node_dead")]
    pub kind: String,
    pub node: Option<String>,
    pub domain: Option<String>,
    #[schema(value_type = Object)]
    pub detail: Value,
}

/// `POST /admin/domains`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DomainRequest {
    pub domain: String,
}
