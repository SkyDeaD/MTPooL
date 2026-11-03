//! Token authentication, admin sessions and the limit on failed attempts.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, MutexGuard};

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use mtpool_core::Ts;
use serde_json::json;

use crate::store::{AuthOp, Store};

pub mod middleware;
pub mod ratelimit;
pub mod session;
pub mod tokens;

/// `last_used_at` of a token is written at most this often.
const TOUCH_EVERY_MS: i64 = 60 * 1000;

/// What the extractors share: the limit on failed attempts and the record of when each token
/// was last marked as used. Clones share the state.
#[derive(Clone, Default)]
pub struct AuthState {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    limiter: Mutex<ratelimit::RateLimiter>,
    last_touch: Mutex<HashMap<i64, Ts>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl AuthState {
    pub fn new() -> AuthState {
        AuthState::default()
    }

    /// Fails with 429 while `ip` is over the limit.
    pub(crate) fn guard(&self, ip: IpAddr, now: Ts) -> Result<(), AuthError> {
        match lock(&self.inner.limiter).blocked(ip, now) {
            None => Ok(()),
            Some(wait) => {
                tracing::warn!(%ip, wait_seconds = wait, "authentication refused: too many failures");
                Err(AuthError::rate_limited(wait))
            }
        }
    }

    /// Counts a failed attempt of `ip`. The reason is a fixed text, never the credential.
    pub(crate) fn failed(&self, ip: IpAddr, now: Ts, reason: &'static str) -> AuthError {
        lock(&self.inner.limiter).record_failure(ip, now);
        tracing::warn!(%ip, reason, "authentication failed");
        AuthError::unauthorized()
    }

    /// Whether `last_used_at` of token `id` is due for a write; if so, it counts as written.
    pub(crate) fn should_touch(&self, id: i64, now: Ts) -> bool {
        let mut seen = lock(&self.inner.last_touch);
        match seen.get(&id) {
            Some(last) if now.0 >= last.0 && now.0 - last.0 < TOUCH_EVERY_MS => false,
            _ => {
                seen.insert(id, now);
                true
            }
        }
    }
}

/// Writes an auth op and waits until it is on disk, so the next request sees it.
pub(crate) async fn commit(store: &Store, op: AuthOp) -> Result<(), AuthError> {
    let store = store.clone();
    let done = tokio::task::spawn_blocking(move || {
        store.send(op);
        store.flush()
    })
    .await;
    match done {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            tracing::error!(error = %e, "cannot write auth data");
            Err(AuthError::internal())
        }
        Err(e) => {
            tracing::error!(error = %e, "auth write task failed");
            Err(AuthError::internal())
        }
    }
}

/// An authentication failure, answered as `application/problem+json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthError {
    status: StatusCode,
    code: &'static str,
    title: &'static str,
    detail: &'static str,
    retry_after: Option<u64>,
}

impl AuthError {
    fn new(
        status: StatusCode,
        code: &'static str,
        title: &'static str,
        detail: &'static str,
    ) -> Self {
        AuthError {
            status,
            code,
            title,
            detail,
            retry_after: None,
        }
    }

    pub fn unauthorized() -> AuthError {
        AuthError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Unauthorized",
            "A valid token is required.",
        )
    }

    pub fn forbidden() -> AuthError {
        AuthError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Forbidden",
            "This token cannot do that.",
        )
    }

    pub fn csrf() -> AuthError {
        AuthError::new(
            StatusCode::FORBIDDEN,
            "csrf",
            "Forbidden",
            "The X-CSRF-Token header is missing or wrong.",
        )
    }

    pub fn rate_limited(retry_after_seconds: u64) -> AuthError {
        AuthError {
            retry_after: Some(retry_after_seconds),
            ..AuthError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "Too many failed attempts",
                "Too many failed attempts from this address; try again later.",
            )
        }
    }

    pub fn bad_request(detail: &'static str) -> AuthError {
        AuthError::new(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "Bad request",
            detail,
        )
    }

    pub fn internal() -> AuthError {
        AuthError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "Internal error",
            "The request could not be processed.",
        )
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let body = json!({
            "type": "about:blank",
            "title": self.title,
            "status": self.status.as_u16(),
            "code": self.code,
            "detail": self.detail,
        });
        let mut response = (self.status, body.to_string()).into_response();
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );
        if let Some(seconds) = self.retry_after {
            headers.insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }
        if self.status == StatusCode::UNAUTHORIZED {
            headers.insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_use_is_written_once_a_minute_per_token() {
        let a = AuthState::new();
        assert!(a.should_touch(1, Ts(1_000)));
        assert!(!a.should_touch(1, Ts(1_000 + 59_999)));
        assert!(a.should_touch(2, Ts(1_001)), "another token is separate");
        assert!(a.should_touch(1, Ts(1_000 + 60_000)));
        assert!(!a.should_touch(1, Ts(1_000 + 60_001)));
    }
}
