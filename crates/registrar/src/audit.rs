//! The audit log of admin actions. Every mutating admin route records one entry.
//!
//! The detail never holds a secret: settings changes log the names of the fields, not their
//! values.

use mtpool_core::Ts;
use serde_json::Value;

use crate::auth::middleware::{AdminAuth, AdminVia};
use crate::store::{AdminOp, Store};

/// Who acted: `token:<id>` for the admin token, `session` for the panel.
pub fn actor_of(auth: &AdminAuth) -> String {
    match &auth.via {
        AdminVia::Token { token_id } => format!("token:{token_id}"),
        AdminVia::Session { .. } => "session".to_owned(),
    }
}

/// Queues an entry. It is written in order with the other writes; a read that must see it
/// flushes the store first.
pub fn record(
    store: &Store,
    at: Ts,
    actor: &str,
    action: &str,
    target: Option<&str>,
    detail: Value,
) {
    store.send(AdminOp::Audit {
        at,
        actor: actor.to_owned(),
        action: action.to_owned(),
        target: target.map(str::to_owned),
        detail: detail.to_string(),
    });
}
