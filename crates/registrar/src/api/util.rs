//! Pieces the bot and admin handlers share.

use std::collections::HashMap;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mtpool_core::{Refusal, Reply};

use super::ApiError;
use super::agent::{empty_ok, json_response};

pub(crate) const DEFAULT_LIMIT: i64 = 50;
pub(crate) const MAX_LIMIT: i64 = 200;

/// `limit` (1 to 200, default 50) and `before` (an id) of a paged list.
pub(crate) fn paging(query: &HashMap<String, String>) -> Result<(i64, Option<i64>), ApiError> {
    let limit = match query.get("limit") {
        None => DEFAULT_LIMIT,
        Some(text) => match text.parse::<i64>() {
            Ok(n) if n >= 1 => n.min(MAX_LIMIT),
            _ => {
                return Err(ApiError::bad_request(
                    "limit must be a number from 1 to 200.",
                ));
            }
        },
    };
    let before = match query.get("before") {
        None => None,
        Some(text) => Some(
            text.parse::<i64>()
                .map_err(|_| ApiError::bad_request("before must be an id."))?,
        ),
    };
    Ok((limit, before))
}

/// A numeric id from a path. One that is not a number names nothing: 404.
pub(crate) fn numeric_id(text: &str, what: &str) -> Result<i64, ApiError> {
    text.parse::<i64>()
        .ok()
        .filter(|id| *id >= 0)
        .ok_or_else(|| ApiError::not_found(&format!("No such {what}.")))
}

/// `n` random bytes as lowercase hex.
pub(crate) fn random_hex(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    rand::fill(&mut bytes[..]);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The answer to a command about a node or a domain that has no content of its own:
///
/// | reply                          | answer                       |
/// |--------------------------------|------------------------------|
/// | `Ok`                           | 200 `{}`                     |
/// | `UnknownNode`, `UnknownDomain` | 404 `not_found`              |
/// | `Stale`                        | 409 `stale`                  |
/// | `NotBanned`                    | 409 `not_banned`             |
/// | `NotRecheckable`               | 409 `not_recheckable`        |
/// | `AlreadyManaged`               | 409 `already_managed`        |
/// | `NotManaged`                   | 409 `not_managed`            |
pub(crate) fn action_answer(reply: Reply) -> Response {
    let conflict = |code: &str, title: &str, detail: &str| {
        ApiError::new(StatusCode::CONFLICT, code, title, detail).into_response()
    };
    match reply {
        Reply::Ok => empty_ok(),
        Reply::UnknownNode => ApiError::not_found("No such node.").into_response(),
        Reply::Stale => conflict(
            "stale",
            "Stale",
            "The node changed since this request was made; try again.",
        ),
        Reply::Refused(Refusal::UnknownDomain) => {
            ApiError::not_found("No such domain.").into_response()
        }
        Reply::Refused(Refusal::NotBanned) => conflict(
            "not_banned",
            "Not banned",
            "The node is not banned, so there is nothing to lift.",
        ),
        Reply::Refused(Refusal::NotRecheckable) => conflict(
            "not_recheckable",
            "Not recheckable",
            "No sensor round is due for the node in its state.",
        ),
        Reply::Refused(Refusal::AlreadyManaged) => conflict(
            "already_managed",
            "Already managed",
            "The domain is managed by SRMD already.",
        ),
        Reply::Refused(Refusal::NotManaged) => conflict(
            "not_managed",
            "Not managed",
            "The domain is not managed by SRMD.",
        ),
        other => {
            tracing::error!(reply = ?other, "API: unexpected reply of the state owner");
            ApiError::internal().into_response()
        }
    }
}

/// A JSON answer with a status.
pub(crate) fn json_status<T: serde::Serialize>(status: StatusCode, body: &T) -> Response {
    json_response(
        status,
        serde_json::to_string(body).expect("an API document serializes"),
    )
}
