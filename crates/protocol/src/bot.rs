//! The bot API under `/api/v1/me`: the owner behind a bot token, its own nodes and its webhook
//! subscriptions.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::public::{Engine, LastRound, NodeState};

/// The owner a bot token belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OwnerInfo {
    pub id: i64,
    pub name: String,
    /// Most nodes the owner may have in the pool at once.
    pub node_limit: i64,
    pub note: String,
}

/// The token a request was made with. The token itself is never shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TokenInfo {
    pub id: i64,
    /// The first characters of the token, to tell tokens apart.
    pub prefix: String,
    /// RFC 3339, UTC.
    pub created_at: String,
    /// Extra rights of the token (`pool:read`); empty for none.
    pub scope: String,
}

/// `GET /me`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Me {
    pub owner: OwnerInfo,
    pub token: TokenInfo,
}

/// A node of the owner, with its full address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OwnNode {
    #[schema(example = "alpha-a1b2c")]
    pub id: String,
    pub state: NodeState,
    pub engine: Engine,
    /// The full address.
    #[schema(example = "203.0.113.7")]
    pub ip: String,
    pub port: u16,
    /// RFC 3339, UTC.
    pub registered_at: String,
    /// When the node entered its current state. RFC 3339, UTC.
    pub state_since: String,
    pub is_master: bool,
    pub domains: Vec<String>,
    /// The latest sensor round; `null` when there was none.
    pub last_round: Option<LastRound>,
}

/// Who a webhook is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WebhookScope {
    /// Events of the owner's nodes.
    Owner,
    /// Events of the whole pool; needs a token with the `pool:read` scope.
    Pool,
}

/// `POST /me/webhooks`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateWebhook {
    /// `https` only, at most 2048 characters, a name (not an address) that resolves to public
    /// addresses only.
    #[schema(example = "https://hooks.example.com/mtpool")]
    pub url: String,
    /// `node.registered`, `node.quarantined`, `node.recovered`, `node.banned`, `node.dead`,
    /// `node.pruned`, `ban.lifted` or `master.changed`.
    pub events: Vec<String>,
    /// The key deliveries are signed with. Generated when absent.
    #[serde(default)]
    pub secret: Option<String>,
    /// `owner` (default) or `pool`.
    #[serde(default)]
    pub scope: Option<WebhookScope>,
}

/// A webhook subscription. The secret is not part of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Webhook {
    pub id: i64,
    pub url: String,
    pub events: Vec<String>,
    pub scope: WebhookScope,
    /// RFC 3339, UTC.
    pub created_at: String,
    /// RFC 3339, UTC; set when the sender gave up on the webhook.
    pub disabled_at: Option<String>,
}

/// The answer to `POST /me/webhooks`: the only time the secret is shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct WebhookCreated {
    pub id: i64,
    pub url: String,
    pub events: Vec<String>,
    pub scope: WebhookScope,
    pub created_at: String,
    pub disabled_at: Option<String>,
    /// Shown once; it cannot be read again.
    pub secret: String,
}

/// The answer to `POST /me/webhooks/{id}/test`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct WebhookTest {
    /// Always `webhook.test`.
    pub event: String,
    /// `pending`: the sender delivers it like any other event.
    pub status: String,
    pub event_id: String,
}

/// One delivery attempt series of an event to a webhook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Delivery {
    pub id: i64,
    pub event_id: String,
    pub event: String,
    /// `pending`, `delivered` or `failed`.
    pub status: String,
    pub attempt: i64,
    pub response_code: Option<i64>,
    pub last_error: Option<String>,
    /// RFC 3339, UTC.
    pub at: String,
    /// When the next attempt is due. RFC 3339, UTC.
    pub next_at: Option<String>,
}
