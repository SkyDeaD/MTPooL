//! Admin sessions: the panel exchanges the admin token for a cookie.
//!
//! The cookie value is 32 random bytes; the database keeps only its SHA-256, plus a CSRF token
//! the page sends in `X-CSRF-Token` on every change. `POST /api/v1/admin/session` logs in,
//! `GET` returns the session's CSRF token again, `DELETE` logs out.

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use mtpool_core::Ts;
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::json;

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

pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/api/v1/admin/session",
        post(login).get(current).delete(logout),
    )
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginBody {
    token: String,
}

fn cookie(value: &str, max_age: i64) -> HeaderValue {
    let text = format!(
        "{COOKIE_NAME}={value}; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age={max_age}"
    );
    HeaderValue::from_str(&text).expect("a cookie of base64url characters is a valid header")
}

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

    let mut response = (StatusCode::OK, json!({ "csrf": csrf }).to_string()).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::SET_COOKIE, cookie(&value, MAX_AGE_SECONDS));
    Ok(response)
}

/// The CSRF token of the session the cookie belongs to, for a panel that was reloaded and lost
/// the one login returned. A bearer token has no session, so there is nothing to return.
async fn current(admin: AdminAuth) -> Response {
    match admin.via {
        AdminVia::Session { csrf, .. } => {
            let mut response =
                (StatusCode::OK, json!({ "csrf": csrf }).to_string()).into_response();
            let headers = response.headers_mut();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
        AdminVia::Token { .. } => StatusCode::NO_CONTENT.into_response(),
    }
}

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
