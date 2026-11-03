//! Handlers of the snapshot-backed lists, one node, the ban log, QR codes and the OpenAPI
//! document.

use std::net::Ipv4Addr;

use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::Response;
use mtpool_core::mask_ip;
use mtpool_protocol::problem::Problem;
use mtpool_protocol::public::{
    BanRecord, BanVote, LastRound, LastVote, ProxyLink, PublicDomain, PublicNode, Verdict, Vote,
};
use mtpool_protocol::rfc3339;
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;

use super::{ApiError, RawQuery, json, qr, query_map, tagged};
use crate::server::AppState;

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 200;

/// Runs `work` on a read-only connection off the async threads.
pub(crate) async fn read<T, F>(app: &AppState, work: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
{
    let store = app.store.clone();
    match tokio::task::spawn_blocking(move || work(&store.read())).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => {
            tracing::error!(error = %e, "public API: cannot read the database");
            Err(ApiError::internal())
        }
        Err(e) => {
            tracing::error!(error = %e, "public API: read task failed");
            Err(ApiError::internal())
        }
    }
}

/// The votes of a `bans.votes` value: a JSON array of `[sensor, vote]` pairs where the vote is
/// `"blocked"`, `"clear"` or `{"abstain": <reason>}`. Anything unreadable is left out.
pub(crate) fn parse_votes(text: &str) -> Vec<BanVote> {
    let Ok(Value::Array(pairs)) = serde_json::from_str::<Value>(text) else {
        tracing::warn!("a ban has votes that are not a JSON array");
        return Vec::new();
    };
    pairs
        .iter()
        .filter_map(|pair| {
            let sensor = pair.get(0)?.as_str()?.to_owned();
            let vote = match pair.get(1)? {
                Value::String(s) if s == "blocked" => Vote::Blocked,
                Value::String(s) if s == "clear" => Vote::Clear,
                Value::Object(o) if o.contains_key("abstain") => Vote::Abstain,
                _ => return None,
            };
            Some(BanVote { sensor, vote })
        })
        .collect()
}

pub(crate) fn mask_text(ip: &str) -> String {
    ip.parse::<Ipv4Addr>()
        .map_or_else(|_| "x.x.x.x".to_owned(), mask_ip)
}

/// A request for a node, domain or link that does not exist, as the 404 answer.
fn unknown(what: &str) -> ApiError {
    ApiError::not_found(&format!("No such {what}."))
}

#[utoipa::path(
    get,
    path = "/api/v1/nodes",
    tag = "public",
    responses(
        (status = 200, description = "All nodes of the pool", body = Vec<PublicNode>,
            headers(("ETag" = String, description = "Changes with the pool; send it back in If-None-Match"))),
        (status = 304, description = "The client's copy is current"),
        (status = 429, description = "Too many requests", body = Problem, content_type = "application/problem+json",
            headers(("Retry-After" = u64, description = "Seconds until the limit window ends"))),
    )
)]
pub async fn list_nodes(State(app): State<AppState>, headers: HeaderMap) -> Response {
    let snapshot = app.snapshots.load();
    tagged(
        &headers,
        &snapshot.etag,
        "application/json",
        snapshot.nodes_json.clone(),
    )
}

#[utoipa::path(
    get,
    path = "/api/v1/nodes/{id}",
    tag = "public",
    params(("id" = String, Path, description = "Node id", example = "alpha-a1b2c")),
    responses(
        (status = 200, description = "The node with its latest sensor round", body = PublicNode),
        (status = 404, description = "No such node", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "Too many requests", body = Problem, content_type = "application/problem+json",
            headers(("Retry-After" = u64, description = "Seconds until the limit window ends"))),
    )
)]
pub async fn get_node(
    State(app): State<AppState>,
    id: Result<Path<String>, PathRejection>,
) -> Result<Response, ApiError> {
    let Ok(Path(id)) = id else {
        return Err(unknown("node"));
    };
    let snapshot = app.snapshots.load();
    let mut node = snapshot
        .node_docs
        .get(&id)
        .cloned()
        .ok_or_else(|| unknown("node"))?;
    node.last_round = read(&app, move |conn| last_round(conn, &id)).await?;
    Ok(json(
        serde_json::to_string(&node).expect("a node serializes"),
    ))
}

/// The latest sensor round of `node`. No evidence and no measurement id leave the database.
pub(crate) fn last_round(conn: &Connection, node: &str) -> rusqlite::Result<Option<LastRound>> {
    let round: Option<(i64, i64, String)> = conn
        .prepare_cached(
            "SELECT id, at, verdict FROM sensor_rounds WHERE node = ?1 ORDER BY at DESC, id DESC LIMIT 1",
        )?
        .query_row([node], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    let Some((round_id, at, verdict)) = round else {
        return Ok(None);
    };
    let Ok(verdict) = serde_json::from_value::<Verdict>(Value::String(verdict)) else {
        tracing::warn!(node, "a sensor round has a verdict the API does not know");
        return Ok(None);
    };
    let rows: Vec<(String, String, String)> = conn
        .prepare_cached(
            "SELECT sensor, vote, data FROM sensor_votes WHERE round_id = ?1 ORDER BY sensor",
        )?
        .query_map([round_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let votes = rows
        .into_iter()
        .filter_map(|(sensor, vote, data)| {
            let vote = serde_json::from_value::<Vote>(Value::String(vote)).ok()?;
            let reason = (vote == Vote::Abstain)
                .then(|| {
                    serde_json::from_str::<Value>(&data)
                        .ok()?
                        .pointer("/vote/abstain")?
                        .as_str()
                        .map(str::to_owned)
                })
                .flatten();
            Some(LastVote {
                sensor,
                vote,
                reason,
            })
        })
        .collect();
    Ok(Some(LastRound {
        at: rfc3339(at),
        verdict,
        votes,
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/domains",
    tag = "public",
    responses(
        (status = 200, description = "The domains of the pool with their masters and DNS", body = Vec<PublicDomain>,
            headers(("ETag" = String, description = "Changes with the pool; send it back in If-None-Match"))),
        (status = 304, description = "The client's copy is current"),
        (status = 429, description = "Too many requests", body = Problem, content_type = "application/problem+json",
            headers(("Retry-After" = u64, description = "Seconds until the limit window ends"))),
    )
)]
pub async fn list_domains(State(app): State<AppState>, headers: HeaderMap) -> Response {
    let snapshot = app.snapshots.load();
    tagged(
        &headers,
        &snapshot.etag,
        "application/json",
        snapshot.domains_json.clone(),
    )
}

#[utoipa::path(
    get,
    path = "/api/v1/links",
    tag = "public",
    responses(
        (status = 200, description = "Telegram proxy links, one per user and domain", body = Vec<ProxyLink>,
            headers(("ETag" = String, description = "Changes with the pool; send it back in If-None-Match"))),
        (status = 304, description = "The client's copy is current"),
        (status = 429, description = "Too many requests", body = Problem, content_type = "application/problem+json",
            headers(("Retry-After" = u64, description = "Seconds until the limit window ends"))),
    )
)]
pub async fn list_links(State(app): State<AppState>, headers: HeaderMap) -> Response {
    let snapshot = app.snapshots.load();
    tagged(
        &headers,
        &snapshot.etag,
        "application/json",
        snapshot.links_json.clone(),
    )
}

#[utoipa::path(
    get,
    path = "/api/v1/links/{id}/qr.svg",
    tag = "public",
    params(("id" = String, Path, description = "Link id, `<user>@<domain>`", example = "alice@proxy.example.com")),
    responses(
        (status = 200, description = "The QR code of the link", content_type = "image/svg+xml", body = String,
            headers(("ETag" = String, description = "The tag of the links list"))),
        (status = 304, description = "The client's copy is current"),
        (status = 404, description = "No such link", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "Too many requests", body = Problem, content_type = "application/problem+json",
            headers(("Retry-After" = u64, description = "Seconds until the limit window ends"))),
    )
)]
pub async fn link_qr(
    State(app): State<AppState>,
    headers: HeaderMap,
    id: Result<Path<String>, PathRejection>,
) -> Result<Response, ApiError> {
    let Ok(Path(id)) = id else {
        return Err(unknown("link"));
    };
    let snapshot = app.snapshots.load();
    let link = snapshot
        .link_index
        .get(&id)
        .ok_or_else(|| unknown("link"))?;
    let Some(picture) = qr::svg(link) else {
        tracing::error!(%id, "a proxy link does not fit in a QR code");
        return Err(ApiError::internal());
    };
    Ok(tagged(&headers, &snapshot.etag, "image/svg+xml", picture))
}

#[utoipa::path(
    get,
    path = "/api/v1/bans",
    tag = "public",
    params(
        ("limit" = Option<i64>, Query, description = "Entries to return, 1 to 200 (default 50)"),
        ("before" = Option<i64>, Query, description = "Only entries with an id below this one: the id of the last entry of the previous page"),
    ),
    responses(
        (status = 200, description = "The ban log, newest first", body = Vec<BanRecord>),
        (status = 400, description = "A parameter is not a number or out of range", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "Too many requests", body = Problem, content_type = "application/problem+json",
            headers(("Retry-After" = u64, description = "Seconds until the limit window ends"))),
    )
)]
pub async fn list_bans(State(app): State<AppState>, query: RawQuery) -> Result<Response, ApiError> {
    let query = query_map(query)?;
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
                .map_err(|_| ApiError::bad_request("before must be a ban id."))?,
        ),
    };
    let bans = read(&app, move |conn| bans_page(conn, limit, before)).await?;
    Ok(json(serde_json::to_string(&bans).expect("bans serialize")))
}

fn bans_page(
    conn: &Connection,
    limit: i64,
    before: Option<i64>,
) -> rusqlite::Result<Vec<BanRecord>> {
    // `gap` looks at the previous ban that is not a re-check, whatever the page holds.
    let mut statement = conn.prepare_cached(
        "WITH real AS (
             SELECT id, LAG(at) OVER (ORDER BY at, id) AS prev_at FROM bans WHERE reverify = 0
         )
         SELECT b.id, b.ip, b.node, b.at, b.votes, b.reverify, b.lifted_at, b.lifted_by_admin,
                CASE WHEN b.reverify = 0 AND n.id IS NOT NULL
                     THEN (b.at - CAST(json_extract(n.record, '$.registered_at') AS INTEGER)) / 1000
                END,
                (b.at - r.prev_at) / 1000
         FROM bans b
         LEFT JOIN nodes n ON n.id = b.node
         LEFT JOIN real r ON r.id = b.id
         WHERE (?1 IS NULL OR b.id < ?1)
         ORDER BY b.id DESC
         LIMIT ?2",
    )?;
    let rows = statement.query_map(rusqlite::params![before, limit], |r| {
        let ip: String = r.get(1)?;
        let votes: String = r.get(4)?;
        let lifted_at: Option<i64> = r.get(6)?;
        Ok(BanRecord {
            id: r.get(0)?,
            ip: mask_text(&ip),
            node_id: r.get(2)?,
            at: rfc3339(r.get(3)?),
            votes: parse_votes(&votes),
            reverify: r.get::<_, i64>(5)? != 0,
            lifted_at: lifted_at.map(rfc3339),
            lifted_by_admin: r.get::<_, i64>(7)? != 0,
            lifetime_sec: r.get(8)?,
            gap_sec: r.get(9)?,
        })
    })?;
    rows.collect()
}

#[utoipa::path(
    get,
    path = "/api/v1/openapi.json",
    tag = "public",
    responses(
        (status = 200, description = "This document", content_type = "application/json", body = Object),
        (status = 429, description = "Too many requests", body = Problem, content_type = "application/problem+json",
            headers(("Retry-After" = u64, description = "Seconds until the limit window ends"))),
    )
)]
pub async fn openapi_document() -> Response {
    let mut response = json(super::openapi_pretty().to_owned());
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}
