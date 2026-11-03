//! Writes of owners, tokens and sessions. They go through the writer like every other write,
//! always as immediate ops: a token or session is readable as soon as the writer has
//! committed it.

use mtpool_core::Ts;
use rusqlite::{Connection, params};

use super::StoreError;

/// A token row to insert. `hash` is SHA-256 of the whole token; the token itself never gets
/// here.
#[derive(Clone, PartialEq, Eq)]
pub struct NewTokenRow {
    pub kind: &'static str,
    pub hash: [u8; 32],
    pub prefix: String,
    pub owner_id: Option<i64>,
    pub node_id: Option<String>,
    pub expires_at: Option<Ts>,
    pub created_at: Ts,
}

#[derive(Clone, PartialEq, Eq)]
pub enum AuthOp {
    InsertOwner {
        name: String,
        node_limit: i64,
        note: String,
        created_at: Ts,
    },
    InsertToken(NewTokenRow),
    RevokeToken {
        id: i64,
        at: Ts,
    },
    TouchToken {
        id: i64,
        at: Ts,
    },
    /// Revokes every admin token, deletes every session and inserts the new admin token, in
    /// one transaction.
    RotateAdmin(NewTokenRow),
    InsertSession {
        hash: [u8; 32],
        csrf: String,
        created_at: Ts,
        expires_at: Ts,
    },
    DeleteSession {
        hash: [u8; 32],
    },
}

// Only the variant name: the ops carry session secrets.
impl std::fmt::Debug for AuthOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(name(self))
    }
}

pub(super) fn name(op: &AuthOp) -> &'static str {
    match op {
        AuthOp::InsertOwner { .. } => "insert_owner",
        AuthOp::InsertToken(_) => "insert_token",
        AuthOp::RevokeToken { .. } => "revoke_token",
        AuthOp::TouchToken { .. } => "touch_token",
        AuthOp::RotateAdmin(_) => "rotate_admin",
        AuthOp::InsertSession { .. } => "insert_session",
        AuthOp::DeleteSession { .. } => "delete_session",
    }
}

fn insert_token(conn: &Connection, t: &NewTokenRow) -> Result<(), StoreError> {
    conn.prepare_cached(
        "INSERT INTO tokens(kind, hash, prefix, owner_id, node_id, expires_at, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?
    .execute(params![
        t.kind,
        t.hash.as_slice(),
        t.prefix,
        t.owner_id,
        t.node_id,
        t.expires_at.map(|e| e.0),
        t.created_at.0,
    ])?;
    Ok(())
}

pub(super) fn apply(conn: &Connection, op: &AuthOp) -> Result<(), StoreError> {
    match op {
        AuthOp::InsertOwner {
            name,
            node_limit,
            note,
            created_at,
        } => {
            conn.prepare_cached(
                "INSERT INTO owners(name, node_limit, note, created_at) VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![name, node_limit, note, created_at.0])?;
        }
        AuthOp::InsertToken(t) => insert_token(conn, t)?,
        AuthOp::RevokeToken { id, at } => {
            conn.prepare_cached(
                "UPDATE tokens SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL",
            )?
            .execute(params![id, at.0])?;
        }
        AuthOp::TouchToken { id, at } => {
            conn.prepare_cached("UPDATE tokens SET last_used_at = ?2 WHERE id = ?1")?
                .execute(params![id, at.0])?;
        }
        AuthOp::RotateAdmin(t) => {
            conn.prepare_cached(
                "UPDATE tokens SET revoked_at = ?1 WHERE kind = 'admin' AND revoked_at IS NULL",
            )?
            .execute([t.created_at.0])?;
            conn.prepare_cached("DELETE FROM sessions")?.execute([])?;
            insert_token(conn, t)?;
        }
        AuthOp::InsertSession {
            hash,
            csrf,
            created_at,
            expires_at,
        } => {
            // Sessions past their end are of no use to anyone: clear them out on the way.
            conn.prepare_cached("DELETE FROM sessions WHERE expires_at <= ?1")?
                .execute([created_at.0])?;
            conn.prepare_cached(
                "INSERT INTO sessions(hash, csrf, created_at, expires_at) VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![hash.as_slice(), csrf, created_at.0, expires_at.0])?;
        }
        AuthOp::DeleteSession { hash } => {
            conn.prepare_cached("DELETE FROM sessions WHERE hash = ?1")?
                .execute([hash.as_slice()])?;
        }
    }
    Ok(())
}
