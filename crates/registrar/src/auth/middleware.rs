//! Extractors that authenticate a request: `AdminAuth`, `BotAuth`, `NodeAuth`, `EnrollAuth`
//! and `MetricsAuth`. A handler that takes one is reachable only with the right token.
//!
//! Failures: no credential or one that does not check out is 401, a valid token of another
//! kind 403, an address over the limit of failed attempts 429. Only credentials that were
//! presented and failed count towards the limit.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::extract::{ConnectInfo, FromRef, FromRequestParts};
use axum::http::header::{AUTHORIZATION, COOKIE};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method};
use mtpool_core::{NodeId, OwnerId};
use subtle::ConstantTimeEq;

use super::AuthError;
use super::session::{self, COOKIE_NAME, CSRF_HEADER};
use super::tokens::{self, TokenHash, TokenKind, TokenRow};
use crate::server::AppState;
use crate::store::AuthOp;

/// The address of the client: the last entry of `X-Forwarded-For` when the peer is the local
/// proxy (loopback), else the peer itself. Anyone else could write that header, so it counts
/// only from the proxy, which appends the address it saw.
pub fn client_ip(parts: &Parts, peer: SocketAddr) -> IpAddr {
    if !peer.ip().is_loopback() {
        return peer.ip();
    }
    parts
        .headers
        .get_all("x-forwarded-for")
        .iter()
        .next_back()
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit(',').next())
        .and_then(|last| last.trim().parse().ok())
        .unwrap_or(peer.ip())
}

/// The address of the client, as `client_ip` decides it. Without connection info (the server
/// was not started with it) the peer is unknown and no header is trusted.
#[derive(Debug, Clone, Copy)]
pub struct ClientIp(pub IpAddr);

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<ClientIp, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map_or(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0), |c| c.0);
        Ok(ClientIp(client_ip(parts, peer)))
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == COOKIE_NAME)
        .map(|(_, value)| value)
        .filter(|v| !v.is_empty())
}

/// Checks a token string: known, live, of the `expected` kind. Counts a failure against `ip`.
pub(crate) async fn verify(
    app: &AppState,
    ip: IpAddr,
    token: &str,
    expected: TokenKind,
) -> Result<TokenRow, AuthError> {
    let now = tokens::now();
    let store = app.store.clone();
    let owned = token.to_owned();
    let found = tokio::task::spawn_blocking(move || tokens::find(&store.read(), &owned))
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "token lookup task failed");
            AuthError::internal()
        })?
        .map_err(|e| {
            tracing::error!(error = %e, "token lookup failed");
            AuthError::internal()
        })?;
    let Some(row) = found else {
        return Err(app.auth.failed(ip, now, "unknown token"));
    };
    if !row.is_live(now) {
        let reason = if row.revoked_at.is_some() {
            "revoked token"
        } else {
            "expired token"
        };
        return Err(app.auth.failed(ip, now, reason));
    }
    if row.kind != expected {
        // A valid token used where it has no power: refused, but not a guess.
        tracing::warn!(%ip, kind = row.kind.as_str(), "token of the wrong kind");
        return Err(AuthError::forbidden());
    }
    if app.auth.should_touch(row.id, now) {
        // Not awaited: when it is lost the only harm is an older `last_used_at`.
        app.store.send(AuthOp::TouchToken {
            id: row.id,
            at: now,
        });
    }
    Ok(row)
}

/// The bearer-token flow shared by the extractors of every kind but admin.
async fn bearer_auth<S>(
    parts: &mut Parts,
    state: &S,
    kind: TokenKind,
) -> Result<TokenRow, AuthError>
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    let app = AppState::from_ref(state);
    let ClientIp(ip) = ClientIp::from_request_parts(parts, state)
        .await
        .unwrap_or_else(|e| match e {});
    app.auth.guard(ip, tokens::now())?;
    let Some(token) = bearer(&parts.headers) else {
        return Err(AuthError::unauthorized());
    };
    verify(&app, ip, token, kind).await
}

fn missing_scope(what: &str) -> AuthError {
    tracing::error!(what, "a token row lacks the scope its kind requires");
    AuthError::internal()
}

/// How an admin request authenticated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminVia {
    /// `Authorization: Bearer <admin token>`: for the command line and API clients.
    Token { token_id: i64 },
    /// The panel's session cookie, with the CSRF token checked on changes.
    Session { hash: TokenHash },
}

/// An admin: the admin token as a bearer, or a panel session.
#[derive(Debug, Clone)]
pub struct AdminAuth {
    pub via: AdminVia,
}

fn is_safe(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

impl<S> FromRequestParts<S> for AdminAuth
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<AdminAuth, AuthError> {
        let app = AppState::from_ref(state);
        let ClientIp(ip) = ClientIp::from_request_parts(parts, state)
            .await
            .unwrap_or_else(|e| match e {});
        app.auth.guard(ip, tokens::now())?;

        if let Some(token) = bearer(&parts.headers) {
            let row = verify(&app, ip, token, TokenKind::Admin).await?;
            return Ok(AdminAuth {
                via: AdminVia::Token { token_id: row.id },
            });
        }
        let Some(cookie) = session_cookie(&parts.headers) else {
            return Err(AuthError::unauthorized());
        };
        let hash = TokenHash::of(cookie);
        let store = app.store.clone();
        let found = tokio::task::spawn_blocking(move || session::lookup(&store.read(), &hash))
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "session lookup task failed");
                AuthError::internal()
            })?
            .map_err(|e| {
                tracing::error!(error = %e, "session lookup failed");
                AuthError::internal()
            })?;
        // A stale cookie (ended, rotated away) is not a guess at a secret of 256 bits: it is
        // refused without counting against the address.
        let Some(session) = found.filter(|s| tokens::now() < s.expires_at) else {
            return Err(AuthError::unauthorized());
        };
        if !is_safe(&parts.method) {
            let sent = parts
                .headers
                .get(CSRF_HEADER)
                .map_or(&[][..], |v| v.as_bytes());
            let matches: bool = sent.ct_eq(session.csrf.as_bytes()).into();
            if !matches {
                return Err(AuthError::csrf());
            }
        }
        Ok(AdminAuth {
            via: AdminVia::Session { hash },
        })
    }
}

/// A bot token: reads the owner's nodes, with webhooks.
#[derive(Debug, Clone)]
pub struct BotAuth {
    pub token_id: i64,
    pub owner_id: OwnerId,
}

impl<S> FromRequestParts<S> for BotAuth
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<BotAuth, AuthError> {
        let row = bearer_auth(parts, state, TokenKind::Bot).await?;
        let owner_id = row.owner.ok_or_else(|| missing_scope("bot owner"))?;
        Ok(BotAuth {
            token_id: row.id,
            owner_id,
        })
    }
}

/// A node token: acts for that node only.
#[derive(Debug, Clone)]
pub struct NodeAuth {
    pub node_id: NodeId,
}

impl<S> FromRequestParts<S> for NodeAuth
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<NodeAuth, AuthError> {
        let row = bearer_auth(parts, state, TokenKind::Node).await?;
        let node_id = row.node.ok_or_else(|| missing_scope("node id"))?;
        Ok(NodeAuth { node_id })
    }
}

/// An enroll token: `POST /agent/enroll` for one owner.
#[derive(Debug, Clone)]
pub struct EnrollAuth {
    pub owner_id: OwnerId,
    pub token_id: i64,
}

impl<S> FromRequestParts<S> for EnrollAuth
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<EnrollAuth, AuthError> {
        let row = bearer_auth(parts, state, TokenKind::Enroll).await?;
        let owner_id = row.owner.ok_or_else(|| missing_scope("enroll owner"))?;
        Ok(EnrollAuth {
            owner_id,
            token_id: row.id,
        })
    }
}

/// A metrics token: `/metrics` only.
#[derive(Debug, Clone)]
pub struct MetricsAuth {
    pub token_id: i64,
}

impl<S> FromRequestParts<S> for MetricsAuth
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<MetricsAuth, AuthError> {
        let row = bearer_auth(parts, state, TokenKind::Metrics).await?;
        Ok(MetricsAuth { token_id: row.id })
    }
}
