//! API tokens: format, hashing, issuing and lookup.
//!
//! A token looks like `mtp_<kind>_<random>`, the random part being 32 bytes in base64url
//! without padding. Only the SHA-256 of the whole token is stored; the token itself is shown
//! once, when it is issued.

use data_encoding::BASE64URL_NOPAD;
use mtpool_core::{NodeId, OwnerId, Ts};
use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::store::{AuthOp, NewTokenRow, Store, StoreError, now_ts};

/// Bytes of randomness in a token (and in a session cookie).
pub const RANDOM_BYTES: usize = 32;
/// Leading characters of a token kept in clear, to tell tokens apart in the panel.
pub const PREFIX_LEN: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenKind {
    /// Panel login and everything under `/admin`.
    Admin,
    /// Only `POST /agent/enroll`, for one owner.
    Enroll,
    /// Issued at enrollment; acts for that one node.
    Node,
    /// Read access to an owner's nodes, and webhooks.
    Bot,
    /// Only `/metrics`.
    Metrics,
}

impl TokenKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TokenKind::Admin => "admin",
            TokenKind::Enroll => "enroll",
            TokenKind::Node => "node",
            TokenKind::Bot => "bot",
            TokenKind::Metrics => "metrics",
        }
    }

    pub fn parse(s: &str) -> Option<TokenKind> {
        Some(match s {
            "admin" => TokenKind::Admin,
            "enroll" => TokenKind::Enroll,
            "node" => TokenKind::Node,
            "bot" => TokenKind::Bot,
            "metrics" => TokenKind::Metrics,
            _ => return None,
        })
    }
}

/// SHA-256 of a token or a session cookie value.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TokenHash([u8; 32]);

impl TokenHash {
    pub fn of(secret: &str) -> TokenHash {
        TokenHash(Sha256::digest(secret.as_bytes()).into())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Comparison in constant time.
    pub fn ct_eq(&self, other: &TokenHash) -> bool {
        self.0.ct_eq(&other.0).into()
    }
}

// Not printed: a hash is not a secret, but it is a key to a secret's record.
impl std::fmt::Debug for TokenHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TokenHash(..)")
    }
}

pub fn now() -> Ts {
    now_ts()
}

/// 32 random bytes from the operating system's seeded generator, as base64url.
pub(crate) fn random_secret() -> String {
    let mut bytes = [0u8; RANDOM_BYTES];
    rand::fill(&mut bytes);
    BASE64URL_NOPAD.encode(&bytes)
}

pub fn new_token(kind: TokenKind) -> (String, TokenHash) {
    let token = format!("mtp_{}_{}", kind.as_str(), random_secret());
    let hash = TokenHash::of(&token);
    (token, hash)
}

/// The kind a token string claims, if it has the shape `mtp_<kind>_<43 base64url chars>`.
pub fn parse_kind(token: &str) -> Option<TokenKind> {
    let (kind, random) = token.strip_prefix("mtp_")?.split_once('_')?;
    let well_formed = random.len() == BASE64URL_NOPAD.encode_len(RANDOM_BYTES)
        && random
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if well_formed {
        TokenKind::parse(kind)
    } else {
        None
    }
}

/// What a new token is for.
#[derive(Debug, Clone)]
pub struct NewToken {
    pub kind: TokenKind,
    /// Required for enroll and bot tokens, none for the others.
    pub owner: Option<OwnerId>,
    /// Required for node tokens, none for the others.
    pub node: Option<NodeId>,
    pub expires_at: Option<Ts>,
}

fn invalid(msg: &str) -> StoreError {
    StoreError::InvalidData(msg.to_owned())
}

fn row_for(kind: TokenKind, token: &str, hash: TokenHash) -> NewTokenRow {
    NewTokenRow {
        kind: kind.as_str(),
        hash: *hash.as_bytes(),
        prefix: token.chars().take(PREFIX_LEN).collect(),
        owner_id: None,
        node_id: None,
        expires_at: None,
        created_at: now(),
    }
}

/// Issues a token and returns it: the only time it exists in clear. Blocks until the token is
/// on disk, so call it from `spawn_blocking` in async code.
pub fn issue(store: &Store, new: NewToken) -> Result<String, StoreError> {
    let scope_ok = match new.kind {
        TokenKind::Admin | TokenKind::Metrics => new.owner.is_none() && new.node.is_none(),
        TokenKind::Enroll | TokenKind::Bot => new.owner.is_some() && new.node.is_none(),
        TokenKind::Node => new.owner.is_none() && new.node.is_some(),
    };
    if !scope_ok {
        return Err(invalid("the token's owner or node does not fit its kind"));
    }
    if let Some(owner) = new.owner {
        let known: bool = store.read().query_row(
            "SELECT EXISTS(SELECT 1 FROM owners WHERE id = ?1)",
            [owner.0],
            |r| r.get(0),
        )?;
        if !known {
            return Err(invalid("unknown owner"));
        }
    }
    let (token, hash) = new_token(new.kind);
    let mut row = row_for(new.kind, &token, hash);
    row.owner_id = new.owner.map(|o| o.0);
    row.node_id = new.node.map(|n| n.as_str().to_owned());
    row.expires_at = new.expires_at;
    store.send(AuthOp::InsertToken(row));
    store.flush()?;
    Ok(token)
}

/// Issues a new admin token and, in the same transaction, revokes the old ones and ends every
/// session. Blocks until it is on disk.
pub fn rotate_admin(store: &Store) -> Result<String, StoreError> {
    let (token, hash) = new_token(TokenKind::Admin);
    store.send(AuthOp::RotateAdmin(row_for(TokenKind::Admin, &token, hash)));
    store.flush()?;
    Ok(token)
}

pub fn revoke(store: &Store, id: i64, at: Ts) -> Result<(), StoreError> {
    store.send(AuthOp::RevokeToken { id, at });
    store.flush()
}

/// Adds an owner and returns its id.
pub fn create_owner(
    store: &Store,
    name: &str,
    node_limit: i64,
    note: &str,
) -> Result<OwnerId, StoreError> {
    let taken: bool = store.read().query_row(
        "SELECT EXISTS(SELECT 1 FROM owners WHERE name = ?1)",
        [name],
        |r| r.get(0),
    )?;
    if taken {
        return Err(invalid("an owner with this name exists"));
    }
    store.send(AuthOp::InsertOwner {
        name: name.to_owned(),
        node_limit,
        note: note.to_owned(),
        created_at: now(),
    });
    store.flush()?;
    let id = store
        .read()
        .query_row("SELECT id FROM owners WHERE name = ?1", [name], |r| {
            r.get(0)
        })?;
    Ok(OwnerId(id))
}

#[derive(Debug, Clone)]
pub struct TokenRow {
    pub id: i64,
    pub kind: TokenKind,
    pub hash: TokenHash,
    pub owner: Option<OwnerId>,
    pub node: Option<NodeId>,
    pub expires_at: Option<Ts>,
    pub revoked_at: Option<Ts>,
    pub last_used_at: Option<Ts>,
}

impl TokenRow {
    /// Neither revoked nor past its expiry at `now`.
    pub fn is_live(&self, now: Ts) -> bool {
        self.revoked_at.is_none() && self.expires_at.is_none_or(|e| now < e)
    }
}

/// The token row with this hash.
pub fn lookup(conn: &Connection, hash: &TokenHash) -> Result<Option<TokenRow>, StoreError> {
    let raw = conn
        .prepare_cached(
            "SELECT id, kind, hash, owner_id, node_id, expires_at, revoked_at, last_used_at \
             FROM tokens WHERE hash = ?1",
        )?
        .query_row([hash.as_bytes().as_slice()], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<i64>>(5)?,
                r.get::<_, Option<i64>>(6)?,
                r.get::<_, Option<i64>>(7)?,
            ))
        })
        .optional()?;
    let Some((id, kind, stored, owner, node, expires, revoked, used)) = raw else {
        return Ok(None);
    };
    let stored: [u8; 32] = stored
        .try_into()
        .map_err(|_| invalid("a stored token hash is not 32 bytes"))?;
    let row_hash = TokenHash(stored);
    // The lookup went by the hash already; the comparison in constant time is the check.
    if !row_hash.ct_eq(hash) {
        return Ok(None);
    }
    Ok(Some(TokenRow {
        id,
        kind: TokenKind::parse(&kind)
            .ok_or_else(|| invalid("a stored token has an unknown kind"))?,
        hash: row_hash,
        owner: owner.map(OwnerId),
        node: node
            .map(|n| NodeId::parse(&n))
            .transpose()
            .map_err(|_| invalid("a stored token has an invalid node id"))?,
        expires_at: expires.map(Ts),
        revoked_at: revoked.map(Ts),
        last_used_at: used.map(Ts),
    }))
}

/// The row of the token string `token`, if it is a known token.
pub fn find(conn: &Connection, token: &str) -> Result<Option<TokenRow>, StoreError> {
    if parse_kind(token).is_none() {
        return Ok(None);
    }
    lookup(conn, &TokenHash::of(token))
}
