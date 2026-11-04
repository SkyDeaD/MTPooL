//! Admin sessions: the panel exchanges the admin token for a cookie.
//!
//! The cookie value is 32 random bytes; the database keeps only its SHA-256, plus a CSRF token
//! the page sends in `X-CSRF-Token` on every change. `POST /api/v1/admin/session` logs in,
//! `GET` returns the session's CSRF token again, `DELETE` logs out.

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use mtpool_core::Ts;
use mtpool_protocol::problem::Problem;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::middleware::{AdminAuth, AdminVia, ClientIp, verify};
use super::tokens::{self, TokenHash, TokenKind};
use super::{AuthError, commit};
use crate::server::AppState;
use crate::store::{AuthOp, StoreError};

pub const COOKIE_NAME: &str = "mtpool_session";
pub const CSRF_HEADER: &str = "x-csrf-token";
/// A session lasts 7 days.
pub const SESSION_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;
const MAX_AGE_SECONDS: i64 = SESSION_TTL_MS / 1000;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(login, current, logout))
}

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub csrf: String,
    pub expires_at: Ts,
}

/// The session whose cookie hashes to `hash`, ended or not.
pub fn lookup(conn: &Connection, hash: &TokenHash) -> Result<Option<SessionRow>, StoreError> {
    Ok(conn
        .prepare_cached("SELECT csrf, expires_at FROM sessions WHERE hash = ?1")?
        .query_row([hash.as_bytes().as_slice()], |r| {
            Ok(SessionRow {
                csrf: r.get(0)?,
                expires_at: Ts(r.get(1)?),
            })
        })
        .optional()?)
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct LoginBody {
    /// The admin token.
    token: String,
}

/// What the panel sends back in `X-CSRF-Token` on every change.
#[derive(Serialize, ToSchema)]
struct SessionCsrf {
    csrf: String,
}

fn csrf_answer(csrf: String) -> Response {
    let mut response = (StatusCode::OK, Json(SessionCsrf { csrf })).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn cookie(value: &str, max_age: i64) -> HeaderValue {
    let text = format!(
        "{COOKIE_NAME}={value}; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age={max_age}"
    );
    HeaderValue::from_str(&text).expect("a cookie of base64url characters is a valid header")
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/session",
    tag = "admin",
    request_body = LoginBody,
    responses(
        (status = 200, description = "Logged in: the session cookie is set and the CSRF token returned", body = SessionCsrf),
        (status = 400, description = "Bad body", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not a valid admin token", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "Too many failed attempts from this address", body = Problem, content_type = "application/problem+json"),
    )
)]
async fn login(
    State(app): State<AppState>,
    ClientIp(ip): ClientIp,
    body: Bytes,
) -> Result<Response, AuthError> {
    app.auth.guard(ip, tokens::now())?;
    let body: LoginBody = serde_json::from_slice(&body)
        .map_err(|_| AuthError::bad_request("Expected a JSON body {\"token\": \"...\"}."))?;
    verify(&app, ip, &body.token, TokenKind::Admin).await?;

    let value = tokens::random_secret();
    let csrf = tokens::random_secret();
    let now = tokens::now();
    commit(
        &app.store,
        AuthOp::InsertSession {
            hash: *TokenHash::of(&value).as_bytes(),
            csrf: csrf.clone(),
            created_at: now,
            expires_at: Ts(now.0 + SESSION_TTL_MS),
        },
    )
    .await?;

    let mut response = csrf_answer(csrf);
    response
        .headers_mut()
        .insert(header::SET_COOKIE, cookie(&value, MAX_AGE_SECONDS));
    Ok(response)
}

/// The CSRF token of the session the cookie belongs to, for a panel that was reloaded and lost
/// the one login returned. A bearer token has no session, so there is nothing to return.
#[utoipa::path(
    get,
    path = "/api/v1/admin/session",
    tag = "admin",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "The CSRF token of the session the cookie belongs to", body = SessionCsrf),
        (status = 204, description = "Signed in with a bearer token, which has no session"),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
    )
)]
async fn current(admin: AdminAuth) -> Response {
    match admin.via {
        AdminVia::Session { csrf, .. } => csrf_answer(csrf),
        AdminVia::Token { .. } => StatusCode::NO_CONTENT.into_response(),
    }
}

#[utoipa::path(
    delete,
    path = "/api/v1/admin/session",
    tag = "admin",
    security(("bearer" = [])),
    responses(
        (status = 204, description = "Logged out; the session cookie is cleared"),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
    )
)]
async fn logout(State(app): State<AppState>, admin: AdminAuth) -> Result<Response, AuthError> {
    if let AdminVia::Session { hash, .. } = admin.via {
        commit(
            &app.store,
            AuthOp::DeleteSession {
                hash: *hash.as_bytes(),
            },
        )
        .await?;
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, cookie("", 0));
    Ok(response)
}
