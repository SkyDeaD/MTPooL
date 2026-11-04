//! Writes of the admin and bot API: audit, webhooks, secrets and owner edits. Like every
//! other write they go through the writer as immediate ops.

use mtpool_core::Ts;
use rusqlite::{Connection, params};

use super::StoreError;

#[derive(Clone, PartialEq)]
pub enum AdminOp {
    Audit {
        at: Ts,
        actor: String,
        action: String,
        target: Option<String>,
        /// A JSON document, already serialized.
        detail: String,
    },
    InsertWebhook {
        owner_id: i64,
        /// `owner` or `pool`.
        scope: String,
        url: String,
        secret: String,
        /// A JSON array, already serialized.
        events: String,
        created_at: Ts,
    },
    DeleteWebhook {
        id: i64,
    },
    InsertDelivery {
        webhook_id: i64,
        event_id: String,
        event: String,
        payload: String,
        at: Ts,
    },
    /// The outcome of one send of a delivery.
    DeliveryResult {
        id: i64,
        /// `pending`, `delivered` or `failed`.
        status: String,
        attempt: i64,
        response_code: Option<i64>,
        /// A short fixed code (`http_500`, `connect_error`, ...), never text of the receiver.
        last_error: Option<String>,
        next_at: Option<Ts>,
    },
    SetSecret {
        name: String,
        value: String,
        at: Ts,
    },
    DeleteSecret {
        name: String,
    },
    UpdateOwner {
        id: i64,
        name: Option<String>,
        node_limit: Option<i64>,
        note: Option<String>,
    },
    /// Soft delete: revokes the owner's tokens, deletes its webhooks and frees its name.
    DeleteOwner {
        id: i64,
        at: Ts,
    },
    /// Inserts or rewrites a rollout (`state` is its JSON document, already serialized).
    SaveRollout {
        id: i64,
        version: String,
        state: String,
        created_at: Ts,
        updated_at: Ts,
    },
}

// Only the variant name: the ops carry webhook and API secrets.
impl std::fmt::Debug for AdminOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(name(self))
    }
}

pub(super) fn name(op: &AdminOp) -> &'static str {
    match op {
        AdminOp::Audit { .. } => "audit",
        AdminOp::InsertWebhook { .. } => "insert_webhook",
        AdminOp::DeleteWebhook { .. } => "delete_webhook",
        AdminOp::InsertDelivery { .. } => "insert_delivery",
        AdminOp::DeliveryResult { .. } => "delivery_result",
        AdminOp::SetSecret { .. } => "set_secret",
        AdminOp::DeleteSecret { .. } => "delete_secret",
        AdminOp::UpdateOwner { .. } => "update_owner",
        AdminOp::DeleteOwner { .. } => "delete_owner",
        AdminOp::SaveRollout { .. } => "save_rollout",
    }
}

/// Applies the op and returns the id of the row an insert made (0 for the other ops).
pub(super) fn apply(conn: &Connection, op: &AdminOp) -> Result<i64, StoreError> {
    match op {
        AdminOp::Audit {
            at,
            actor,
            action,
            target,
            detail,
        } => {
            conn.prepare_cached(
                "INSERT INTO audit(at, actor, action, target, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?
            .execute(params![at.0, actor, action, target, detail])?;
            Ok(conn.last_insert_rowid())
        }
        AdminOp::InsertWebhook {
            owner_id,
            scope,
            url,
            secret,
            events,
            created_at,
        } => {
            conn.prepare_cached(
                "INSERT INTO webhooks(owner_id, scope, url, secret, events, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(params![owner_id, scope, url, secret, events, created_at.0])?;
            Ok(conn.last_insert_rowid())
        }
        AdminOp::DeleteWebhook { id } => {
            conn.prepare_cached("DELETE FROM webhook_deliveries WHERE webhook_id = ?1")?
                .execute([id])?;
            conn.prepare_cached("DELETE FROM webhooks WHERE id = ?1")?
                .execute([id])?;
            Ok(0)
        }
        AdminOp::InsertDelivery {
            webhook_id,
            event_id,
            event,
            payload,
            at,
        } => {
            conn.prepare_cached(
                "INSERT INTO webhook_deliveries(webhook_id, event_id, event, payload, status, at, next_at) \
                 VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?5)",
            )?
            .execute(params![webhook_id, event_id, event, payload, at.0])?;
            Ok(conn.last_insert_rowid())
        }
        AdminOp::DeliveryResult {
            id,
            status,
            attempt,
            response_code,
            last_error,
            next_at,
        } => {
            conn.prepare_cached(
                "UPDATE webhook_deliveries SET status = ?2, attempt = ?3, response_code = ?4, \
                 last_error = ?5, next_at = ?6 WHERE id = ?1",
            )?
            .execute(params![
                id,
                status,
                attempt,
                response_code,
                last_error,
                next_at.map(|t| t.0)
            ])?;
            Ok(0)
        }
        AdminOp::SaveRollout {
            id,
            version,
            state,
            created_at,
            updated_at,
        } => {
            conn.prepare_cached(
                "INSERT INTO rollouts(id, version, state, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(id) DO UPDATE SET state = excluded.state, updated_at = excluded.updated_at",
            )?
            .execute(params![id, version, state, created_at.0, updated_at.0])?;
            Ok(*id)
        }
        AdminOp::SetSecret { name, value, at } => {
            conn.prepare_cached(
                "INSERT INTO secrets(name, value, updated_at) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(name) DO UPDATE SET value = excluded.value, \
                 updated_at = excluded.updated_at",
            )?
            .execute(params![name, value, at.0])?;
            Ok(0)
        }
        AdminOp::DeleteSecret { name } => {
            conn.prepare_cached("DELETE FROM secrets WHERE name = ?1")?
                .execute([name])?;
            Ok(0)
        }
        AdminOp::UpdateOwner {
            id,
            name,
            node_limit,
            note,
        } => {
            conn.prepare_cached(
                "UPDATE owners SET name = COALESCE(?2, name), \
                 node_limit = COALESCE(?3, node_limit), note = COALESCE(?4, note) \
                 WHERE id = ?1 AND deleted_at IS NULL",
            )?
            .execute(params![id, name, node_limit, note])?;
            Ok(0)
        }
        AdminOp::DeleteOwner { id, at } => {
            conn.prepare_cached(
                "UPDATE tokens SET revoked_at = ?2 WHERE owner_id = ?1 AND revoked_at IS NULL",
            )?
            .execute(params![id, at.0])?;
            conn.prepare_cached(
                "DELETE FROM webhook_deliveries WHERE webhook_id IN \
                 (SELECT id FROM webhooks WHERE owner_id = ?1)",
            )?
            .execute([id])?;
            conn.prepare_cached("DELETE FROM webhooks WHERE owner_id = ?1")?
                .execute([id])?;
            conn.prepare_cached(
                "UPDATE owners SET deleted_at = ?2, name = name || '~deleted~' || id \
                 WHERE id = ?1 AND deleted_at IS NULL",
            )?
            .execute(params![id, at.0])?;
            Ok(0)
        }
    }
}
