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

/// The note of a rollout that waits or is paused: why, in a code and in words for the admin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RolloutNote {
    /// `no_canary_candidate` (waiting), `node_not_back`, `health_drop`, `update_failed`,
    /// `bad_signature` or `rolled_back` (paused), `superseded` (cancelled by a newer release).
    #[schema(example = "node_not_back")]
    pub code: String,
    pub text: String,
}

/// The timings a rollout runs by; copied from the settings when it starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RolloutTimings {
    pub observe_sec: u32,
    pub batch_interval_sec: u32,
    pub batch_percent: u32,
    pub return_sec: u32,
    pub offer_wait_sec: u32,
}

/// How many nodes are at each stage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RolloutCounts {
    pub idle: u32,
    pub offered: u32,
    pub started: u32,
    pub updated: u32,
    pub failed: u32,
    pub missed: u32,
}

/// One node in a rollout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RolloutNode {
    pub node_id: String,
    /// The agent version the node last reported; absent until it has polled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// `idle`, `offered`, `started`, `updated`, `failed` or `missed`.
    pub stage: String,
    /// RFC 3339: when the node entered the stage (absent for `idle`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    /// With `failed`: the code the agent or the registrar gave.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// A rollout of an agent release: `GET /admin/rollouts` and `/admin/rollouts/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RolloutView {
    pub id: i64,
    /// The release version being rolled out.
    pub version: String,
    /// RFC 3339.
    pub created_at: String,
    /// `canary` (one node takes it; waits without a candidate), `observing` (the canary is
    /// watched), `batches`, `paused`, `done` or `cancelled`.
    pub phase: String,
    /// With `paused`: the phase "continue" goes back to (`canary`, `observing`, `batches`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_to: Option<String>,
    /// RFC 3339: when the canary is judged good (`observing`: its start; add `observe_sec`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observing_since: Option<String>,
    /// RFC 3339: when the next batch goes out (`batches`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_batch_at: Option<String>,
    /// RFC 3339: when the rollout was paused, finished or cancelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_at: Option<String>,
    /// Why the rollout waits or is paused; absent while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<RolloutNote>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canary: Option<String>,
    /// Whether `POST .../resume` is accepted (the rollout is paused).
    pub can_resume: bool,
    /// Whether `POST .../cancel` is accepted (the rollout is not over).
    pub can_cancel: bool,
    pub timings: RolloutTimings,
    pub counts: RolloutCounts,
    pub nodes: Vec<RolloutNode>,
}
