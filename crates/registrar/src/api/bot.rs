//! The bot API under `/api/v1/me`: the owner behind a bot token, its own nodes and its webhooks.
//!
//! A bot token acts for one owner. A node that is not the owner's is answered exactly as one
//! that does not exist (404 `not_found`), never 403, so ids of other owners cannot be probed.
//! The public request limit does not apply; like the agent API these routes are held to the
//! limit on failed attempts.
//!
//! | route                                 | answer                                          |
//! |---------------------------------------|-------------------------------------------------|
//! | `GET /me`                             | 200 `{owner, token}`                            |
//! | `GET /me/nodes`                       | 200 `[OwnNode]`, full addresses                 |
//! | `POST /me/nodes/{id}/retire`          | 200 `{}`                                        |
//! | `POST /me/nodes/{id}/recheck`         | 200 `{}`, 429 `rate_limited`, 409 `not_recheckable` |
//! | `GET /me/webhooks`                    | 200 `[Webhook]`                                 |
//! | `POST /me/webhooks`                   | 201 `WebhookCreated` (the secret, once)         |
//! | `DELETE /me/webhooks/{id}`            | 200 `{}`                                        |
//! | `POST /me/webhooks/{id}/test`         | 202 `WebhookTest`                               |
//! | `GET /me/webhooks/{id}/deliveries`    | 200 `[Delivery]`, newest first                  |

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use mtpool_core::{Command, NodeAction, NodeId, ReplyId, Ts};
use mtpool_protocol::agent::{Empty, EmptyRequest};
use mtpool_protocol::bot::{
    CreateWebhook, Delivery, Me, OwnNode, OwnerInfo, TokenInfo, Webhook, WebhookCreated,
    WebhookScope, WebhookTest,
};
use mtpool_protocol::problem::Problem;
use mtpool_protocol::rfc3339;
use rusqlite::{OptionalExtension, params};
use serde_json::json;
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::ApiError;
use super::agent::{Failure, actor_stopped, blocking, body_bytes, empty_ok, parse, parse_empty};
use super::public::{last_round, read};
use super::util::{action_answer, json_status, numeric_id, paging, random_hex};
use super::webhook_url;
use crate::auth::middleware::BotAuth;
use crate::auth::tokens::SCOPE_POOL_READ;
use crate::server::AppState;
use crate::snapshot::View;
use crate::store::AdminOp;
use crate::webhooks;

/// A node is re-checked at most this often through the bot API.
pub const RECHECK_EVERY_MS: i64 = 10 * 60 * 1000;
/// The webhook events a subscription may name.
pub const WEBHOOK_EVENTS: &[&str] = &[
    "node.registered",
    "node.quarantined",
    "node.recovered",
    "node.banned",
    "node.dead",
    "node.pruned",
    "ban.lifted",
    "master.changed",
];
/// The event a test delivery carries.
pub const TEST_EVENT: &str = "webhook.test";

const SECRET_MIN: usize = 16;
const SECRET_MAX: usize = 256;

#[derive(OpenApi)]
#[openapi(
    tags((name = "bot", description = "Routes of an owner's bot; they need a bot token (`mtp_bot_...`)")),
    components(schemas(Problem))
)]
struct BotDoc;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::with_openapi(BotDoc::openapi())
        .routes(routes!(me))
        .routes(routes!(list_nodes))
        .routes(routes!(retire_node))
        .routes(routes!(recheck_node))
        .routes(routes!(list_webhooks, create_webhook))
        .routes(routes!(delete_webhook))
        .routes(routes!(test_webhook))
        .routes(routes!(list_deliveries))
}

fn not_found(what: &str) -> Failure {
    ApiError::not_found(&format!("No such {what}.")).into()
}

// ----- me ----------------------------------------------------------------------------------

#[utoipa::path(
    get,
    path = "/api/v1/me",
    tag = "bot",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "The owner and the token", body = Me),
        (status = 401, description = "No valid bot token", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A token of another kind", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn me(State(app): State<AppState>, auth: BotAuth) -> Result<Response, Failure> {
    let owner_id = auth.owner_id.0;
    let token_id = auth.token_id;
    let found = read(&app, move |conn| {
        let owner = conn
            .query_row(
                "SELECT id, name, node_limit, note FROM owners WHERE id = ?1 AND deleted_at IS NULL",
                [owner_id],
                |r| {
                    Ok(OwnerInfo {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        node_limit: r.get(2)?,
                        note: r.get(3)?,
                    })
                },
            )
            .optional()?;
        let token = conn
            .query_row(
                "SELECT id, prefix, created_at, scope FROM tokens WHERE id = ?1",
                [token_id],
                |r| {
                    Ok(TokenInfo {
                        id: r.get(0)?,
                        prefix: r.get(1)?,
                        created_at: rfc3339(r.get(2)?),
                        scope: r.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(owner.zip(token))
    })
    .await?;
    let (owner, token) = found.ok_or_else(|| not_found("owner"))?;
    Ok(json_status(StatusCode::OK, &Me { owner, token }))
}

// ----- nodes -------------------------------------------------------------------------------

/// The node when it is this owner's; a node of another owner is the same 404 as a missing one.
async fn own_node(app: &AppState, auth: &BotAuth, id: &str) -> Result<NodeId, Failure> {
    let node = NodeId::parse(id).map_err(|_| not_found("node"))?;
    let owner = auth.owner_id;
    let asked = node.clone();
    let owned = app
        .actor
        .inspect(move |state| state.nodes.get(&asked).is_some_and(|n| n.owner == owner))
        .await
        .map_err(|_| actor_stopped())?;
    if owned {
        Ok(node)
    } else {
        Err(not_found("node"))
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/me/nodes",
    operation_id = "list_my_nodes",
    tag = "bot",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "The owner's nodes, with full addresses", body = Vec<OwnNode>),
        (status = 401, description = "No valid bot token", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A token of another kind", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn list_nodes(State(app): State<AppState>, auth: BotAuth) -> Result<Response, Failure> {
    let owner = auth.owner_id;
    let mut nodes = app
        .actor
        .inspect(move |state| {
            let view = View::new(state);
            state
                .nodes
                .values()
                .filter(|n| n.owner == owner)
                .map(|n| {
                    let public = view.node(n);
                    OwnNode {
                        id: public.id,
                        state: public.state,
                        engine: public.engine,
                        ip: n.ip.to_string(),
                        port: public.port,
                        registered_at: public.registered_at,
                        state_since: public.state_since,
                        is_master: public.is_master,
                        domains: public.domains,
                        last_round: None,
                    }
                })
                .collect::<Vec<OwnNode>>()
        })
        .await
        .map_err(|_| actor_stopped())?;
    nodes.sort_by(|a, b| a.id.cmp(&b.id));
    let nodes = read(&app, move |conn| {
        for node in &mut nodes {
            node.last_round = last_round(conn, &node.id)?;
        }
        Ok(nodes)
    })
    .await?;
    Ok(json_status(StatusCode::OK, &nodes))
}

#[utoipa::path(
    post,
    path = "/api/v1/me/nodes/{id}/retire",
    operation_id = "retire_my_node",
    tag = "bot",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "The node id")),
    request_body(content = EmptyRequest, description = "Empty, or `{}`"),
    responses(
        (status = 200, description = "The node is out of the pool", body = Empty),
        (status = 400, description = "Bad body", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No valid bot token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such node of this owner", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn retire_node(
    State(app): State<AppState>,
    auth: BotAuth,
    Path(id): Path<String>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Failure> {
    parse_empty(&body_bytes(body)?)?;
    let node = own_node(&app, &auth, &id).await?;
    let reply = app
        .actor
        .call(Command::Retire(NodeAction {
            reply: ReplyId(0),
            node,
        }))
        .await
        .map_err(|_| actor_stopped())?;
    Ok(action_answer(reply))
}

/// Notes a re-check of `node` at `now`. `Err` holds the seconds until the next one is allowed.
/// `Ok` holds what was there before, to put it back when the re-check is refused.
fn claim_recheck(app: &AppState, node: &NodeId, now: Ts) -> Result<Option<Ts>, u64> {
    let mut asked = app.rechecks.lock().unwrap_or_else(|e| e.into_inner());
    asked.retain(|_, at| now.0.saturating_sub(at.0) < RECHECK_EVERY_MS);
    if let Some(at) = asked.get(node) {
        let left_ms = RECHECK_EVERY_MS - now.0.saturating_sub(at.0);
        return Err(u64::try_from((left_ms + 999) / 1000)
            .unwrap_or(1)
            .clamp(1, 600));
    }
    Ok(asked.insert(node.clone(), now))
}

fn unclaim_recheck(app: &AppState, node: &NodeId, before: Option<Ts>) {
    let mut asked = app.rechecks.lock().unwrap_or_else(|e| e.into_inner());
    match before {
        Some(at) => asked.insert(node.clone(), at),
        None => asked.remove(node),
    };
}

#[utoipa::path(
    post,
    path = "/api/v1/me/nodes/{id}/recheck",
    operation_id = "recheck_my_node",
    tag = "bot",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "The node id")),
    request_body(content = EmptyRequest, description = "Empty, or `{}`"),
    responses(
        (status = 200, description = "A sensor round is asked for", body = Empty),
        (status = 400, description = "Bad body", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No valid bot token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such node of this owner", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "No round is due for the node (`not_recheckable`)", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "Asked less than 10 minutes ago", body = Problem, content_type = "application/problem+json",
            headers(("Retry-After" = u64, description = "Seconds until the next one is allowed"))),
    )
)]
pub async fn recheck_node(
    State(app): State<AppState>,
    auth: BotAuth,
    Path(id): Path<String>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Failure> {
    parse_empty(&body_bytes(body)?)?;
    let node = own_node(&app, &auth, &id).await?;
    let now = (app.clock)();
    let before = match claim_recheck(&app, &node, now) {
        Ok(before) => before,
        Err(seconds) => return Err(ApiError::rate_limited(seconds).into()),
    };
    let reply = app
        .actor
        .call(Command::Recheck(NodeAction {
            reply: ReplyId(0),
            node: node.clone(),
        }))
        .await;
    let reply = match reply {
        Ok(reply) => reply,
        Err(_) => {
            unclaim_recheck(&app, &node, before);
            return Err(actor_stopped());
        }
    };
    if !matches!(reply, mtpool_core::Reply::Ok) {
        // Nothing was started, so it does not use up the allowance.
        unclaim_recheck(&app, &node, before);
    }
    Ok(action_answer(reply))
}

// ----- webhooks ----------------------------------------------------------------------------

fn scope_text(scope: WebhookScope) -> &'static str {
    match scope {
        WebhookScope::Owner => "owner",
        WebhookScope::Pool => "pool",
    }
}

fn webhook_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Webhook> {
    let events: String = r.get(2)?;
    let scope: String = r.get(3)?;
    Ok(Webhook {
        id: r.get(0)?,
        url: r.get(1)?,
        events: serde_json::from_str(&events).unwrap_or_default(),
        scope: if scope == "pool" {
            WebhookScope::Pool
        } else {
            WebhookScope::Owner
        },
        created_at: rfc3339(r.get(4)?),
    })
}

#[utoipa::path(
    get,
    path = "/api/v1/me/webhooks",
    tag = "bot",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "The owner's webhooks, without secrets", body = Vec<Webhook>),
        (status = 401, description = "No valid bot token", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn list_webhooks(
    State(app): State<AppState>,
    auth: BotAuth,
) -> Result<Response, Failure> {
    let owner = auth.owner_id.0;
    let hooks = read(&app, move |conn| {
        conn.prepare_cached(
            "SELECT id, url, events, scope, created_at FROM webhooks \
             WHERE owner_id = ?1 ORDER BY id",
        )?
        .query_map([owner], webhook_row)?
        .collect::<rusqlite::Result<Vec<Webhook>>>()
    })
    .await?;
    Ok(json_status(StatusCode::OK, &hooks))
}

/// A subscription's event list: not empty, no repeats, only known names.
fn check_events(events: &[String]) -> Result<(), ApiError> {
    if events.is_empty() {
        return Err(ApiError::bad_request(
            "events must name at least one event.",
        ));
    }
    for (i, event) in events.iter().enumerate() {
        if !WEBHOOK_EVENTS.contains(&event.as_str()) {
            return Err(ApiError::bad_request(&format!(
                "Unknown event name; the known ones are {}.",
                WEBHOOK_EVENTS.join(", ")
            )));
        }
        if events[..i].contains(event) {
            return Err(ApiError::bad_request("events names an event twice."));
        }
    }
    Ok(())
}

fn check_secret(secret: &str) -> Result<(), ApiError> {
    let len = secret.chars().count();
    if !(SECRET_MIN..=SECRET_MAX).contains(&len) || secret.chars().any(char::is_control) {
        return Err(ApiError::bad_request(
            "secret must be 16 to 256 characters without control characters.",
        ));
    }
    Ok(())
}

#[utoipa::path(
    post,
    path = "/api/v1/me/webhooks",
    tag = "bot",
    security(("bearer" = [])),
    request_body = CreateWebhook,
    responses(
        (status = 201, description = "The webhook; the secret is shown only here", body = WebhookCreated),
        (status = 400, description = "Bad body, unknown event (`bad_request`) or a URL that is refused (`webhook_url`)", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No valid bot token", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Scope `pool` needs the token scope `pool:read`", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn create_webhook(
    State(app): State<AppState>,
    auth: BotAuth,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Failure> {
    let request: CreateWebhook = parse(&body_bytes(body)?)?;
    check_events(&request.events)?;
    if let Some(secret) = &request.secret {
        check_secret(secret)?;
    }
    let scope = request.scope.unwrap_or(WebhookScope::Owner);
    if scope == WebhookScope::Pool && auth.scope != SCOPE_POOL_READ {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Forbidden",
            "A pool-wide webhook needs a token with the pool:read scope.",
        )
        .into());
    }
    webhook_url::check(app.resolver.as_ref(), &request.url)
        .await
        .map_err(|refused| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "webhook_url",
                "Webhook URL refused",
                &refused.to_string(),
            )
        })?;
    let secret = request
        .secret
        .unwrap_or_else(|| format!("whsec_{}", random_hex(24)));
    let created_at = (app.clock)();
    let op = AdminOp::InsertWebhook {
        owner_id: auth.owner_id.0,
        scope: scope_text(scope).to_owned(),
        url: request.url.clone(),
        secret: secret.clone(),
        events: serde_json::to_string(&request.events).expect("a list of names serializes"),
        created_at,
    };
    let store = app.store.clone();
    let id = blocking(move || store.admin(op)).await?.map_err(|e| {
        tracing::error!(error = %e, "bot API: cannot store a webhook");
        ApiError::internal()
    })?;
    Ok(json_status(
        StatusCode::CREATED,
        &WebhookCreated {
            id,
            url: request.url,
            events: request.events,
            scope,
            created_at: rfc3339(created_at.0),
            secret,
        },
    ))
}

/// The id of a webhook of this owner; another owner's is a 404.
async fn own_webhook(app: &AppState, auth: &BotAuth, id: &str) -> Result<i64, Failure> {
    let id = numeric_id(id, "webhook")?;
    let owner = auth.owner_id.0;
    let owned = read(app, move |conn| {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM webhooks WHERE id = ?1 AND owner_id = ?2)",
            params![id, owner],
            |r| r.get::<_, bool>(0),
        )
    })
    .await?;
    if owned {
        Ok(id)
    } else {
        Err(not_found("webhook"))
    }
}

#[utoipa::path(
    delete,
    path = "/api/v1/me/webhooks/{id}",
    tag = "bot",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "The webhook id")),
    responses(
        (status = 200, description = "The webhook and its deliveries are gone", body = Empty),
        (status = 401, description = "No valid bot token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such webhook of this owner", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn delete_webhook(
    State(app): State<AppState>,
    auth: BotAuth,
    Path(id): Path<String>,
) -> Result<Response, Failure> {
    let id = own_webhook(&app, &auth, &id).await?;
    let store = app.store.clone();
    blocking(move || store.admin(AdminOp::DeleteWebhook { id }))
        .await?
        .map_err(|e| {
            tracing::error!(error = %e, "bot API: cannot delete a webhook");
            ApiError::internal()
        })?;
    Ok(empty_ok())
}

#[utoipa::path(
    post,
    path = "/api/v1/me/webhooks/{id}/test",
    tag = "bot",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "The webhook id")),
    request_body(content = EmptyRequest, description = "Empty, or `{}`"),
    responses(
        (status = 202, description = "A `webhook.test` delivery is queued. Its body has the shape of every delivery: `{\"id\": \"evt_...\", \"event\": \"webhook.test\", \"at\": <RFC 3339>, \"node\": null, \"domain\": null, \"detail\": {\"webhook_id\": <id>}}`; `id` equals the `event_id` of this answer", body = WebhookTest),
        (status = 400, description = "Bad body", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No valid bot token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such webhook of this owner", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn test_webhook(
    State(app): State<AppState>,
    auth: BotAuth,
    Path(id): Path<String>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Failure> {
    parse_empty(&body_bytes(body)?)?;
    let id = own_webhook(&app, &auth, &id).await?;
    let at = (app.clock)();
    let event_id = webhooks::new_event_id();
    let payload = webhooks::payload(
        &event_id,
        TEST_EVENT,
        at,
        None,
        None,
        &json!({ "webhook_id": id }),
    );
    let op = AdminOp::InsertDelivery {
        webhook_id: id,
        event_id: event_id.clone(),
        event: TEST_EVENT.to_owned(),
        payload,
        at,
    };
    let store = app.store.clone();
    blocking(move || store.admin(op)).await?.map_err(|e| {
        tracing::error!(error = %e, "bot API: cannot queue a test delivery");
        ApiError::internal()
    })?;
    Ok(json_status(
        StatusCode::ACCEPTED,
        &WebhookTest {
            event: TEST_EVENT.to_owned(),
            status: "pending".to_owned(),
            event_id,
        },
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/me/webhooks/{id}/deliveries",
    tag = "bot",
    security(("bearer" = [])),
    params(
        ("id" = String, Path, description = "The webhook id"),
        ("limit" = Option<i64>, Query, description = "1 to 200, default 50"),
        ("before" = Option<i64>, Query, description = "Only deliveries with a smaller id"),
    ),
    responses(
        (status = 200, description = "Deliveries, newest first", body = Vec<Delivery>),
        (status = 400, description = "Bad query", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No valid bot token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such webhook of this owner", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn list_deliveries(
    State(app): State<AppState>,
    auth: BotAuth,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, Failure> {
    let (limit, before) = paging(&query)?;
    let id = own_webhook(&app, &auth, &id).await?;
    let deliveries = read(&app, move |conn| {
        conn.prepare_cached(
            "SELECT id, event_id, event, status, attempt, response_code, last_error, at, next_at \
             FROM webhook_deliveries WHERE webhook_id = ?1 AND (?2 IS NULL OR id < ?2) \
             ORDER BY id DESC LIMIT ?3",
        )?
        .query_map(params![id, before, limit], |r| {
            Ok(Delivery {
                id: r.get(0)?,
                event_id: r.get(1)?,
                event: r.get(2)?,
                status: r.get(3)?,
                attempt: r.get(4)?,
                response_code: r.get(5)?,
                last_error: r.get(6)?,
                at: rfc3339(r.get(7)?),
                next_at: r.get::<_, Option<i64>>(8)?.map(rfc3339),
            })
        })?
        .collect::<rusqlite::Result<Vec<Delivery>>>()
    })
    .await?;
    Ok(json_status(StatusCode::OK, &deliveries))
}
