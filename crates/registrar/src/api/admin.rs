//! The admin API under `/api/v1/admin`: settings, owners, tokens, node and domain actions, the
//! audit log, the event log and the live event stream.
//!
//! Every route needs the admin token (or the panel session with its CSRF header). Every route
//! that changes something writes one entry to the audit log; the detail of an entry never holds
//! a secret. The public request limit does not apply.
//!
//! Settings: `GET /admin/settings` is the settings document and `secrets`, a map of the secret
//! names to whether a value is available. `PATCH /admin/settings` takes a partial document:
//! the fields present are merged into the live settings (nested objects field by field, lists
//! replaced), the result must pass `Settings::validate`, and only then is anything applied.
//! The optional `secrets` object sets (`"value"`) or clears (`null`) a secret; secrets are
//! write-only. A value set here wins over the environment variable of the same service.
//!
//! | route                                        | answer                                  |
//! |----------------------------------------------|-----------------------------------------|
//! | `GET/PATCH /admin/settings`                  | 200 settings and `secrets`              |
//! | `GET/POST /admin/owners`                     | 200 list, 201 owner, 409 `owner_exists` |
//! | `PATCH/DELETE /admin/owners/{id}`            | 200, 404, 409 `owner_has_nodes`         |
//! | `GET/POST /admin/tokens`                     | 200 metadata, 201 with the value once   |
//! | `DELETE /admin/tokens/{id}`                  | 200, 404                                |
//! | `POST /admin/nodes/{id}/retire|recheck|unban`| 200, 404, 409                           |
//! | `GET/POST /admin/domains`, `DELETE ../{d}`   | 200, 201, 404, 409 `domain_exists`      |
//! | `POST /admin/srmd/{domain}/take|release`     | 200, 404, 409                           |
//! | `GET /admin/audit`, `GET /admin/events`      | 200, newest first, paged by `before`    |
//! | `GET /admin/stream`                          | `text/event-stream`                     |

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use mtpool_core::{
    Command, NodeAction, NodeId, NodeState, OwnerId, ReplyId, Settings, SettingsError,
    SrmdManualAction, SrmdManualRequest, Ts, failed_conditions, needed_domains,
};
use mtpool_protocol::admin::{
    AdminNode, AdminNodeDetail, AdminRound, AdminStatus, AdminVote, AuditEntry, DnsStatus,
    DomainRequest, EventEntry, NewOwner, NewToken as NewTokenRequest, NewTokenCreated, Owner,
    OwnerPatch, SensorStatus, SrmdOverview, StatusCounts, TokenMeta,
};
use mtpool_protocol::agent::{Empty, EmptyRequest};
use mtpool_protocol::problem::Problem;
use mtpool_protocol::public::{Verdict, Vote};
use mtpool_protocol::{parse_rfc3339, rfc3339};
use rusqlite::{OptionalExtension, params};
use serde_json::{Map, Value, json};
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::ApiError;
use super::agent::{Failure, actor_stopped, blocking, body_bytes, empty_ok, parse, parse_empty};
use super::public::read;
use super::util::{action_answer, json_status, numeric_id, paging};
use super::webhook_url::is_dns_name;
use crate::audit;
use crate::auth::middleware::AdminAuth;
use crate::auth::tokens::{self, NewToken, PREFIX_LEN, SCOPE_POOL_READ, TokenKind};
use crate::secrets;
use crate::server::AppState;
use crate::store::{AdminOp, StoreError};

const MAX_OWNER_NAME: usize = 64;
const MAX_OWNER_NOTE: usize = 500;
const MAX_NODE_LIMIT: i64 = 100_000;
const MAX_SECRET_LEN: usize = 4096;

#[derive(OpenApi)]
#[openapi(
    tags((name = "admin", description = "Routes of the panel and the admin; they need the admin token (`mtp_admin_...`) or a panel session")),
    components(schemas(Problem))
)]
struct AdminDoc;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::with_openapi(AdminDoc::openapi())
        .routes(routes!(get_settings, patch_settings))
        .routes(routes!(list_owners, create_owner))
        .routes(routes!(patch_owner, delete_owner))
        .routes(routes!(list_tokens, create_token))
        .routes(routes!(revoke_token))
        .routes(routes!(retire_node))
        .routes(routes!(recheck_node))
        .routes(routes!(unban_node))
        .routes(routes!(list_domains, add_domain))
        .routes(routes!(remove_domain))
        .routes(routes!(srmd_take))
        .routes(routes!(srmd_release))
        .routes(routes!(list_audit))
        .routes(routes!(list_events))
        .routes(routes!(stream))
        .routes(routes!(list_nodes))
        .routes(routes!(get_node))
        .routes(routes!(status))
        .routes(routes!(srmd_overview))
        .routes(routes!(dns_push))
}

// ----- helpers -----------------------------------------------------------------------------

fn not_found(what: &str) -> Failure {
    ApiError::not_found(&format!("No such {what}.")).into()
}

fn conflict(code: &str, title: &str, detail: &str) -> Failure {
    ApiError::new(StatusCode::CONFLICT, code, title, detail).into()
}

fn storage_failed(what: &str, e: &StoreError) -> Failure {
    tracing::error!(error = %e, "admin API: {what}");
    ApiError::internal().into()
}

/// Writes one audit entry now.
fn record(app: &AppState, auth: &AdminAuth, action: &str, target: Option<&str>, detail: Value) {
    audit::record(
        &app.store,
        (app.clock)(),
        &audit::actor_of(auth),
        action,
        target,
        detail,
    );
}

/// Waits until everything queued before is on disk, so that a read sees it.
async fn flushed(app: &AppState) -> Result<(), Failure> {
    let store = app.store.clone();
    blocking(move || store.flush())
        .await?
        .map_err(|e| storage_failed("cannot flush the store", &e))
}

/// The node id of a path; one that is not an id names nothing.
fn node_of(id: &str) -> Result<NodeId, Failure> {
    NodeId::parse(id).map_err(|_| not_found("node"))
}

// ----- settings ----------------------------------------------------------------------------

async fn live_settings(app: &AppState) -> Result<Settings, Failure> {
    app.actor
        .inspect(|state| state.settings.clone())
        .await
        .map_err(|_| actor_stopped())
}

/// The settings document with the `secrets` map.
async fn settings_document(app: &AppState, settings: &Settings) -> Result<Value, Failure> {
    let mut document = serde_json::to_value(settings).expect("settings serialize");
    let store = app.store.clone();
    let present = blocking(move || {
        secrets::names()
            .map(|name| (name.to_owned(), Value::Bool(secrets::is_set(&store, name))))
            .collect::<Map<String, Value>>()
    })
    .await?;
    if let Value::Object(fields) = &mut document {
        fields.insert("secrets".to_owned(), Value::Object(present));
    }
    Ok(document)
}

fn invalid_settings(e: &SettingsError) -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "invalid_settings",
        "Invalid settings",
        &e.to_string(),
    )
}

/// Hands valid settings to the state owner. Callers hold `settings_lock`.
async fn commit_settings(app: &AppState, settings: Settings) -> Result<(), Failure> {
    settings.validate().map_err(|e| invalid_settings(&e))?;
    let globalping = settings.globalping.clone();
    app.actor
        .call(Command::SettingsChanged(settings))
        .await
        .map_err(|_| actor_stopped())?;
    // The verifier and the sensor read the Globalping settings from here.
    app.gp_settings.store(Arc::new(globalping));
    Ok(())
}

/// Merges `patch` into `base`: objects field by field, everything else replaced.
fn merge(base: &mut Value, patch: &Value) {
    match (base, patch) {
        (Value::Object(base), Value::Object(patch)) => {
            for (key, value) in patch {
                match base.get_mut(key) {
                    Some(slot) if slot.is_object() && value.is_object() => merge(slot, value),
                    _ => {
                        base.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (slot, value) => *slot = value.clone(),
    }
}

fn clipped(name: &str) -> String {
    name.chars().filter(|c| !c.is_control()).take(64).collect()
}

/// The names of the fields a patch sets; nested ones as `parent.child`. Refuses a field the
/// settings do not have.
fn patched_fields(patch: &Map<String, Value>) -> Result<Vec<String>, ApiError> {
    let known = serde_json::to_value(Settings::default()).expect("settings serialize");
    let Value::Object(known) = known else {
        unreachable!("settings are an object")
    };
    let mut names = Vec::new();
    for (key, value) in patch {
        let Some(default) = known.get(key) else {
            return Err(ApiError::bad_request(&format!(
                "Unknown setting: {}.",
                clipped(key)
            )));
        };
        match (default, value) {
            (Value::Object(inner), Value::Object(given)) => {
                for nested in given.keys() {
                    if !inner.contains_key(nested) {
                        return Err(ApiError::bad_request(&format!(
                            "Unknown setting: {}.{}.",
                            clipped(key),
                            clipped(nested)
                        )));
                    }
                    names.push(format!("{key}.{nested}"));
                }
            }
            _ => names.push(key.clone()),
        }
    }
    names.sort();
    Ok(names)
}

/// The live settings with `patch` merged in. A value of the wrong type is a 400 that names the
/// top-level field; the text of the parser is not passed on.
fn merged_settings(live: &Settings, patch: &Map<String, Value>) -> Result<Settings, ApiError> {
    let base = serde_json::to_value(live).expect("settings serialize");
    for (key, value) in patch {
        let mut one = base.clone();
        merge(&mut one, &json!({ key: value }));
        if serde_json::from_value::<Settings>(one).is_err() {
            return Err(ApiError::bad_request(&format!(
                "The value of {} has the wrong type.",
                clipped(key)
            )));
        }
    }
    let mut merged = base;
    merge(&mut merged, &Value::Object(patch.clone()));
    serde_json::from_value(merged)
        .map_err(|_| ApiError::bad_request("The settings do not fit together."))
}

/// What `PATCH /admin/settings` asks for: the settings fields and the secrets.
struct SettingsPatch {
    fields: Map<String, Value>,
    /// `Some(value)` sets, `None` clears.
    secrets: Vec<(String, Option<String>)>,
}

fn settings_patch(bytes: &Bytes) -> Result<SettingsPatch, Failure> {
    let Value::Object(mut fields) = parse::<Value>(bytes)? else {
        return Err(ApiError::bad_request("The body must be a JSON object.").into());
    };
    let mut wanted = Vec::new();
    if let Some(given) = fields.remove("secrets") {
        let Value::Object(given) = given else {
            return Err(ApiError::bad_request("secrets must be an object.").into());
        };
        for (name, value) in given {
            if !secrets::is_known(&name) {
                return Err(
                    ApiError::bad_request(&format!("Unknown secret: {}.", clipped(&name))).into(),
                );
            }
            let value = match value {
                Value::Null => None,
                Value::String(text)
                    if !text.is_empty()
                        && text.len() <= MAX_SECRET_LEN
                        && !text.chars().any(char::is_control) =>
                {
                    Some(text)
                }
                _ => {
                    return Err(ApiError::bad_request(&format!(
                        "The value of secret {name} must be a non-empty string without control characters, or null to clear it."
                    ))
                    .into());
                }
            };
            wanted.push((name, value));
        }
    }
    Ok(SettingsPatch {
        fields,
        secrets: wanted,
    })
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/settings",
    tag = "admin",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "The settings document and `secrets`: for each secret whether a value is available (never the value)", body = Object),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn get_settings(
    State(app): State<AppState>,
    _auth: AdminAuth,
) -> Result<Response, Failure> {
    let settings = live_settings(&app).await?;
    Ok(json_status(
        StatusCode::OK,
        &settings_document(&app, &settings).await?,
    ))
}

#[utoipa::path(
    patch,
    path = "/api/v1/admin/settings",
    tag = "admin",
    security(("bearer" = [])),
    request_body(content = Object, description = "A partial settings document; `secrets` maps a secret name to a value or null"),
    responses(
        (status = 200, description = "The new settings document and `secrets`", body = Object),
        (status = 400, description = "Unknown field or secret, wrong type (`bad_request`) or a value settings do not allow (`invalid_settings`); nothing was changed", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn patch_settings(
    State(app): State<AppState>,
    auth: AdminAuth,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Failure> {
    let patch = settings_patch(&body_bytes(body)?)?;
    let names = patched_fields(&patch.fields)?;
    let _guard = app.settings_lock.lock().await;
    let live = live_settings(&app).await?;
    let merged = merged_settings(&live, &patch.fields)?;
    merged.validate().map_err(|e| invalid_settings(&e))?;
    // Everything is checked: now the secrets, then the settings.
    let at = (app.clock)();
    for (name, value) in &patch.secrets {
        let op = match value {
            Some(value) => AdminOp::SetSecret {
                name: name.clone(),
                value: value.clone(),
                at,
            },
            None => AdminOp::DeleteSecret { name: name.clone() },
        };
        let store = app.store.clone();
        blocking(move || store.admin(op))
            .await?
            .map_err(|e| storage_failed("cannot store a secret", &e))?;
    }
    if !patch.fields.is_empty() {
        commit_settings(&app, merged).await?;
    }
    if !names.is_empty() || !patch.secrets.is_empty() {
        let mut touched: Vec<&str> = patch.secrets.iter().map(|(n, _)| n.as_str()).collect();
        touched.sort_unstable();
        record(
            &app,
            &auth,
            "settings.update",
            None,
            json!({ "fields": names, "secrets": touched }),
        );
    }
    let settings = live_settings(&app).await?;
    Ok(json_status(
        StatusCode::OK,
        &settings_document(&app, &settings).await?,
    ))
}

// ----- owners ------------------------------------------------------------------------------

fn owner_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Owner> {
    Ok(Owner {
        id: r.get(0)?,
        name: r.get(1)?,
        node_limit: r.get(2)?,
        note: r.get(3)?,
        created_at: rfc3339(r.get(4)?),
    })
}

const OWNER_COLUMNS: &str = "id, name, node_limit, note, created_at";

fn check_owner_name(name: &str) -> Result<(), ApiError> {
    let ok = (1..=MAX_OWNER_NAME).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(ApiError::bad_request(
            "name must be 1 to 64 characters: letters, digits, dot, underscore or hyphen.",
        ))
    }
}

fn check_node_limit(limit: i64) -> Result<(), ApiError> {
    if (0..=MAX_NODE_LIMIT).contains(&limit) {
        Ok(())
    } else {
        Err(ApiError::bad_request(
            "node_limit must be from 0 to 100000.",
        ))
    }
}

fn check_owner_note(note: &str) -> Result<(), ApiError> {
    if note.chars().count() <= MAX_OWNER_NOTE {
        Ok(())
    } else {
        Err(ApiError::bad_request(
            "note must be at most 500 characters.",
        ))
    }
}

async fn owner_by_id(app: &AppState, id: i64) -> Result<Option<Owner>, Failure> {
    Ok(read(app, move |conn| {
        conn.query_row(
            &format!("SELECT {OWNER_COLUMNS} FROM owners WHERE id = ?1 AND deleted_at IS NULL"),
            [id],
            owner_row,
        )
        .optional()
    })
    .await?)
}

fn owner_exists() -> Failure {
    conflict(
        "owner_exists",
        "Owner exists",
        "An owner with this name exists already.",
    )
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/owners",
    tag = "admin",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "The owners", body = Vec<Owner>),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn list_owners(
    State(app): State<AppState>,
    _auth: AdminAuth,
) -> Result<Response, Failure> {
    let owners = read(&app, |conn| {
        conn.prepare_cached(&format!(
            "SELECT {OWNER_COLUMNS} FROM owners WHERE deleted_at IS NULL ORDER BY id"
        ))?
        .query_map([], owner_row)?
        .collect::<rusqlite::Result<Vec<Owner>>>()
    })
    .await?;
    Ok(json_status(StatusCode::OK, &owners))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/owners",
    tag = "admin",
    security(("bearer" = [])),
    request_body = NewOwner,
    responses(
        (status = 201, description = "The owner", body = Owner),
        (status = 400, description = "Bad body", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "An owner with this name exists (`owner_exists`)", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn create_owner(
    State(app): State<AppState>,
    auth: AdminAuth,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Failure> {
    let request: NewOwner = parse(&body_bytes(body)?)?;
    check_owner_name(&request.name)?;
    check_node_limit(request.node_limit)?;
    let note = request.note.unwrap_or_default();
    check_owner_note(&note)?;
    let store = app.store.clone();
    let name = request.name.clone();
    let limit = request.node_limit;
    let id = blocking(move || tokens::create_owner(&store, &name, limit, &note))
        .await?
        .map_err(|e| match e {
            StoreError::InvalidData(_) => owner_exists(),
            other => storage_failed("cannot create an owner", &other),
        })?;
    record(
        &app,
        &auth,
        "owner.create",
        Some(&format!("owner:{}", id.0)),
        json!({ "name": request.name, "node_limit": request.node_limit }),
    );
    let owner = owner_by_id(&app, id.0)
        .await?
        .ok_or_else(|| not_found("owner"))?;
    Ok(json_status(StatusCode::CREATED, &owner))
}

#[utoipa::path(
    patch,
    path = "/api/v1/admin/owners/{id}",
    tag = "admin",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "The owner id")),
    request_body = OwnerPatch,
    responses(
        (status = 200, description = "The owner after the change", body = Owner),
        (status = 400, description = "Bad body", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such owner", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The name is taken (`owner_exists`)", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn patch_owner(
    State(app): State<AppState>,
    auth: AdminAuth,
    Path(id): Path<String>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Failure> {
    let id = numeric_id(&id, "owner")?;
    let request: OwnerPatch = parse(&body_bytes(body)?)?;
    if let Some(name) = &request.name {
        check_owner_name(name)?;
    }
    if let Some(limit) = request.node_limit {
        check_node_limit(limit)?;
    }
    if let Some(note) = &request.note {
        check_owner_note(note)?;
    }
    owner_by_id(&app, id)
        .await?
        .ok_or_else(|| not_found("owner"))?;
    if let Some(name) = request.name.clone() {
        let taken = read(&app, move |conn| {
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM owners WHERE name = ?1 AND id != ?2)",
                params![name, id],
                |r| r.get::<_, bool>(0),
            )
        })
        .await?;
        if taken {
            return Err(owner_exists());
        }
    }
    let mut changed: Vec<&str> = Vec::new();
    if request.name.is_some() {
        changed.push("name");
    }
    if request.node_limit.is_some() {
        changed.push("node_limit");
    }
    if request.note.is_some() {
        changed.push("note");
    }
    let op = AdminOp::UpdateOwner {
        id,
        name: request.name,
        node_limit: request.node_limit,
        note: request.note,
    };
    let store = app.store.clone();
    blocking(move || store.admin(op))
        .await?
        .map_err(|e| storage_failed("cannot update an owner", &e))?;
    record(
        &app,
        &auth,
        "owner.update",
        Some(&format!("owner:{id}")),
        json!({ "fields": changed }),
    );
    let owner = owner_by_id(&app, id)
        .await?
        .ok_or_else(|| not_found("owner"))?;
    Ok(json_status(StatusCode::OK, &owner))
}

#[utoipa::path(
    delete,
    path = "/api/v1/admin/owners/{id}",
    tag = "admin",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "The owner id")),
    responses(
        (status = 200, description = "The owner is gone; its tokens are revoked and its webhooks deleted", body = Empty),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such owner", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The owner still has nodes in the pool (`owner_has_nodes`)", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn delete_owner(
    State(app): State<AppState>,
    auth: AdminAuth,
    Path(id): Path<String>,
) -> Result<Response, Failure> {
    let id = numeric_id(&id, "owner")?;
    let owner = owner_by_id(&app, id)
        .await?
        .ok_or_else(|| not_found("owner"))?;
    let live = app
        .actor
        .inspect(move |state| {
            state.nodes.values().any(|n| {
                n.owner == OwnerId(id)
                    && matches!(
                        n.state,
                        NodeState::Pending | NodeState::Healthy | NodeState::Quarantine
                    )
            })
        })
        .await
        .map_err(|_| actor_stopped())?;
    if live {
        return Err(conflict(
            "owner_has_nodes",
            "Owner has nodes",
            "Retire the nodes of this owner first.",
        ));
    }
    let at = (app.clock)();
    let store = app.store.clone();
    blocking(move || store.admin(AdminOp::DeleteOwner { id, at }))
        .await?
        .map_err(|e| storage_failed("cannot delete an owner", &e))?;
    record(
        &app,
        &auth,
        "owner.delete",
        Some(&format!("owner:{id}")),
        json!({ "name": owner.name }),
    );
    Ok(empty_ok())
}

// ----- tokens ------------------------------------------------------------------------------

fn token_meta(r: &rusqlite::Row<'_>) -> rusqlite::Result<TokenMeta> {
    let time = |i: usize| -> rusqlite::Result<Option<String>> {
        Ok(r.get::<_, Option<i64>>(i)?.map(rfc3339))
    };
    Ok(TokenMeta {
        id: r.get(0)?,
        kind: r.get(1)?,
        prefix: r.get(2)?,
        owner_id: r.get(3)?,
        scope: r.get(4)?,
        created_at: rfc3339(r.get(5)?),
        expires_at: time(6)?,
        revoked_at: time(7)?,
        last_used_at: time(8)?,
        node_id: r.get(9)?,
    })
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/tokens",
    tag = "admin",
    security(("bearer" = [])),
    params(
        ("owner" = Option<i64>, Query, description = "Only the tokens of this owner"),
        ("kind" = Option<String>, Query, description = "Only the tokens of this kind: `admin`, `enroll`, `node`, `bot` or `metrics`"),
        ("limit" = Option<i64>, Query, description = "1 to 200, default 50"),
        ("before" = Option<i64>, Query, description = "Only tokens with a smaller id"),
    ),
    responses(
        (status = 200, description = "Token metadata, newest first; never the token or its hash", body = Vec<TokenMeta>),
        (status = 400, description = "Bad query", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn list_tokens(
    State(app): State<AppState>,
    _auth: AdminAuth,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, Failure> {
    let (limit, before) = paging(&query)?;
    let owner = match query.get("owner") {
        None => None,
        Some(text) => Some(
            text.parse::<i64>()
                .map_err(|_| ApiError::bad_request("owner must be an owner id."))?,
        ),
    };
    let kind = match query.get("kind") {
        None => None,
        Some(text) => Some(
            TokenKind::parse(text)
                .map(|_| text.clone())
                .ok_or_else(|| {
                    ApiError::bad_request("kind must be admin, enroll, node, bot or metrics.")
                })?,
        ),
    };
    let rows = read(&app, move |conn| {
        conn.prepare_cached(
            "SELECT id, kind, prefix, owner_id, scope, created_at, expires_at, revoked_at, \
             last_used_at, node_id FROM tokens \
             WHERE (?1 IS NULL OR owner_id = ?1) AND (?2 IS NULL OR id < ?2) \
             AND (?4 IS NULL OR kind = ?4) \
             ORDER BY id DESC LIMIT ?3",
        )?
        .query_map(params![owner, before, limit, kind], token_meta)?
        .collect::<rusqlite::Result<Vec<TokenMeta>>>()
    })
    .await?;
    Ok(json_status(StatusCode::OK, &rows))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/tokens",
    tag = "admin",
    security(("bearer" = [])),
    request_body = NewTokenRequest,
    responses(
        (status = 201, description = "The token; its value is shown only here", body = NewTokenCreated),
        (status = 400, description = "Bad body, a kind that cannot be issued here (`bad_request`) or an owner that does not exist (`unknown_owner`)", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn create_token(
    State(app): State<AppState>,
    auth: AdminAuth,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Failure> {
    let request: NewTokenRequest = parse(&body_bytes(body)?)?;
    let kind = match TokenKind::parse(&request.kind) {
        Some(kind @ (TokenKind::Enroll | TokenKind::Bot | TokenKind::Metrics)) => kind,
        _ => {
            return Err(ApiError::bad_request(
                "kind must be enroll, bot or metrics; admin tokens are rotated and node tokens are issued at enrollment.",
            )
            .into());
        }
    };
    let owner = match (kind, request.owner_id) {
        (TokenKind::Metrics, None) => None,
        (TokenKind::Metrics, Some(_)) => {
            return Err(ApiError::bad_request("A metrics token has no owner.").into());
        }
        (_, None) => {
            return Err(ApiError::bad_request("This kind of token needs an owner_id.").into());
        }
        (_, Some(id)) => Some(id),
    };
    let scope = request.scope.unwrap_or_default();
    if !(scope.is_empty() || (kind == TokenKind::Bot && scope == SCOPE_POOL_READ)) {
        return Err(ApiError::bad_request("scope is empty, or pool:read for a bot token.").into());
    }
    let expires_at = match &request.expires_at {
        None => None,
        Some(text) => {
            let ms = parse_rfc3339(text)
                .ok_or_else(|| ApiError::bad_request("expires_at must be an RFC 3339 time."))?;
            if ms <= (app.clock)().0 {
                return Err(ApiError::bad_request("expires_at must be in the future.").into());
            }
            Some(Ts(ms))
        }
    };
    if let Some(id) = owner
        && owner_by_id(&app, id).await?.is_none()
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "unknown_owner",
            "Unknown owner",
            "There is no owner with this id.",
        )
        .into());
    }
    let store = app.store.clone();
    let new = NewToken {
        kind,
        owner: owner.map(OwnerId),
        node: None,
        expires_at,
        scope: scope.clone(),
    };
    let issued = blocking(move || {
        let token = tokens::issue(&store, new)?;
        let row = tokens::find(&store.read(), &token)?;
        Ok::<_, StoreError>((token, row))
    })
    .await?
    .map_err(|e| storage_failed("cannot issue a token", &e))?;
    let (token, row) = issued;
    let row = row.ok_or_else(|| {
        tracing::error!("admin API: a token just issued is not found");
        Failure::from(ApiError::internal())
    })?;
    let prefix: String = token.chars().take(PREFIX_LEN).collect();
    let id = row.id;
    let created_at = read(&app, move |conn| {
        conn.query_row("SELECT created_at FROM tokens WHERE id = ?1", [id], |r| {
            r.get::<_, i64>(0)
        })
    })
    .await?;
    record(
        &app,
        &auth,
        "token.create",
        Some(&format!("token:{id}")),
        json!({
            "kind": kind.as_str(),
            "owner_id": owner,
            "scope": scope,
            "prefix": prefix,
        }),
    );
    Ok(json_status(
        StatusCode::CREATED,
        &NewTokenCreated {
            id,
            kind: kind.as_str().to_owned(),
            prefix,
            owner_id: owner,
            scope,
            created_at: rfc3339(created_at),
            expires_at: expires_at.map(|t| rfc3339(t.0)),
            token,
        },
    ))
}

#[utoipa::path(
    delete,
    path = "/api/v1/admin/tokens/{id}",
    tag = "admin",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "The token id")),
    responses(
        (status = 200, description = "The token is revoked (already revoked is fine)", body = Empty),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such token", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn revoke_token(
    State(app): State<AppState>,
    auth: AdminAuth,
    Path(id): Path<String>,
) -> Result<Response, Failure> {
    let id = numeric_id(&id, "token")?;
    let kind = read(&app, move |conn| {
        conn.query_row("SELECT kind FROM tokens WHERE id = ?1", [id], |r| {
            r.get::<_, String>(0)
        })
        .optional()
    })
    .await?
    .ok_or_else(|| not_found("token"))?;
    let at = (app.clock)();
    let store = app.store.clone();
    blocking(move || tokens::revoke(&store, id, at))
        .await?
        .map_err(|e| storage_failed("cannot revoke a token", &e))?;
    record(
        &app,
        &auth,
        "token.revoke",
        Some(&format!("token:{id}")),
        json!({ "kind": kind }),
    );
    Ok(empty_ok())
}

// ----- node actions ------------------------------------------------------------------------

/// Runs a command about a node and audits it when it was accepted.
async fn node_action(
    app: AppState,
    auth: AdminAuth,
    id: String,
    action: &'static str,
    command: fn(NodeAction) -> Command,
) -> Result<Response, Failure> {
    let node = node_of(&id)?;
    let reply = app
        .actor
        .call(command(NodeAction {
            reply: ReplyId(0),
            node: node.clone(),
        }))
        .await
        .map_err(|_| actor_stopped())?;
    if matches!(reply, mtpool_core::Reply::Ok) {
        record(&app, &auth, action, Some(node.as_str()), json!({}));
    }
    Ok(action_answer(reply))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/nodes/{id}/retire",
    operation_id = "admin_retire_node",
    tag = "admin",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "The node id")),
    request_body(content = EmptyRequest, description = "Empty, or `{}`"),
    responses(
        (status = 200, description = "The node is out of the pool", body = Empty),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such node", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn retire_node(
    State(app): State<AppState>,
    auth: AdminAuth,
    Path(id): Path<String>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Failure> {
    parse_empty(&body_bytes(body)?)?;
    node_action(app, auth, id, "node.retire", Command::Retire).await
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/nodes/{id}/recheck",
    operation_id = "admin_recheck_node",
    tag = "admin",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "The node id")),
    request_body(content = EmptyRequest, description = "Empty, or `{}`"),
    responses(
        (status = 200, description = "A sensor round is asked for", body = Empty),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such node", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "No round is due for the node (`not_recheckable`)", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn recheck_node(
    State(app): State<AppState>,
    auth: AdminAuth,
    Path(id): Path<String>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Failure> {
    parse_empty(&body_bytes(body)?)?;
    node_action(app, auth, id, "node.recheck", Command::Recheck).await
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/nodes/{id}/unban",
    tag = "admin",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "The node id")),
    request_body(content = EmptyRequest, description = "Empty, or `{}`"),
    responses(
        (status = 200, description = "The ban is lifted. A banned node has no record, so the id may also be one the ban log names; the agent enrolls again by itself", body = Empty),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such node and no open ban under this id", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The node is not banned (`not_banned`)", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn unban_node(
    State(app): State<AppState>,
    auth: AdminAuth,
    Path(id): Path<String>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Failure> {
    parse_empty(&body_bytes(body)?)?;
    let target = unban_target(&app, node_of(&id)?).await?;
    node_action(
        app,
        auth,
        target.to_string(),
        "node.unban",
        Command::AdminUnban,
    )
    .await
}

/// The node to lift the ban by. A block remembers the first node banned on the address; the ban
/// log also names every reverify node that failed there, which have no record either. Their ids
/// lead to the same block.
async fn unban_target(app: &AppState, asked: NodeId) -> Result<NodeId, Failure> {
    let probe = asked.clone();
    let known = app
        .actor
        .inspect(move |s| {
            s.nodes.contains_key(&probe) || s.blocks.iter().any(|b| b.node_id == probe)
        })
        .await
        .map_err(|_| actor_stopped())?;
    if known {
        return Ok(asked);
    }
    flushed(app).await?;
    let name = asked.as_str().to_owned();
    let ip: Option<String> = read(app, move |conn| {
        conn.prepare_cached(
            "SELECT ip FROM bans WHERE node = ?1 AND lifted_at IS NULL ORDER BY id DESC LIMIT 1",
        )?
        .query_row([name], |r| r.get(0))
        .optional()
    })
    .await?;
    let Some(ip) = ip.and_then(|ip| ip.parse::<std::net::Ipv4Addr>().ok()) else {
        return Ok(asked);
    };
    let holder = app
        .actor
        .inspect(move |s| {
            s.blocks
                .iter()
                .find(|b| b.ip == ip)
                .map(|b| b.node_id.clone())
        })
        .await
        .map_err(|_| actor_stopped())?;
    Ok(holder.unwrap_or(asked))
}

// ----- domains and SRMD --------------------------------------------------------------------

#[utoipa::path(
    get,
    path = "/api/v1/admin/domains",
    operation_id = "admin_list_domains",
    tag = "admin",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "The proxy domains", body = Vec<String>),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn list_domains(
    State(app): State<AppState>,
    _auth: AdminAuth,
) -> Result<Response, Failure> {
    let settings = live_settings(&app).await?;
    Ok(json_status(StatusCode::OK, &settings.domains))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/domains",
    tag = "admin",
    security(("bearer" = [])),
    request_body = DomainRequest,
    responses(
        (status = 201, description = "The domain is added", body = DomainRequest),
        (status = 400, description = "Bad body or a domain name that is not valid", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The domain is there already (`domain_exists`)", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn add_domain(
    State(app): State<AppState>,
    auth: AdminAuth,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Failure> {
    let request: DomainRequest = parse(&body_bytes(body)?)?;
    let domain = request.domain.trim().to_ascii_lowercase();
    if !is_dns_name(&domain) {
        return Err(ApiError::bad_request(
            "domain must be a domain name such as proxy.example.com.",
        )
        .into());
    }
    let _guard = app.settings_lock.lock().await;
    let mut settings = live_settings(&app).await?;
    if settings.domains.contains(&domain) {
        return Err(conflict(
            "domain_exists",
            "Domain exists",
            "The domain is in the pool already.",
        ));
    }
    settings.domains.push(domain.clone());
    commit_settings(&app, settings).await?;
    record(&app, &auth, "domain.add", Some(&domain), json!({}));
    Ok(json_status(StatusCode::CREATED, &DomainRequest { domain }))
}

#[utoipa::path(
    delete,
    path = "/api/v1/admin/domains/{domain}",
    tag = "admin",
    security(("bearer" = [])),
    params(("domain" = String, Path, description = "The domain")),
    responses(
        (status = 200, description = "The domain is removed", body = Empty),
        (status = 400, description = "It is the last domain (`invalid_settings`)", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such domain", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn remove_domain(
    State(app): State<AppState>,
    auth: AdminAuth,
    Path(domain): Path<String>,
) -> Result<Response, Failure> {
    let domain = domain.to_ascii_lowercase();
    let _guard = app.settings_lock.lock().await;
    let mut settings = live_settings(&app).await?;
    let before = settings.domains.len();
    settings.domains.retain(|d| *d != domain);
    if settings.domains.len() == before {
        return Err(not_found("domain"));
    }
    commit_settings(&app, settings).await?;
    record(&app, &auth, "domain.remove", Some(&domain), json!({}));
    Ok(empty_ok())
}

async fn srmd_action(
    app: AppState,
    auth: AdminAuth,
    domain: String,
    action: SrmdManualAction,
    name: &'static str,
) -> Result<Response, Failure> {
    let domain = domain.to_ascii_lowercase();
    let reply = app
        .actor
        .call(Command::SrmdManual(SrmdManualRequest {
            reply: ReplyId(0),
            domain: domain.clone(),
            action,
        }))
        .await
        .map_err(|_| actor_stopped())?;
    if matches!(reply, mtpool_core::Reply::Ok) {
        record(&app, &auth, name, Some(&domain), json!({}));
    }
    Ok(action_answer(reply))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/srmd/{domain}/take",
    tag = "admin",
    security(("bearer" = [])),
    params(("domain" = String, Path, description = "The domain")),
    request_body(content = EmptyRequest, description = "Empty, or `{}`"),
    responses(
        (status = 200, description = "SRMD takes the domain", body = Empty),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such domain", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Taken already (`already_managed`)", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn srmd_take(
    State(app): State<AppState>,
    auth: AdminAuth,
    Path(domain): Path<String>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Failure> {
    parse_empty(&body_bytes(body)?)?;
    srmd_action(app, auth, domain, SrmdManualAction::Take, "srmd.take").await
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/srmd/{domain}/release",
    tag = "admin",
    security(("bearer" = [])),
    params(("domain" = String, Path, description = "The domain")),
    request_body(content = EmptyRequest, description = "Empty, or `{}`"),
    responses(
        (status = 200, description = "SRMD lets go of the domain", body = Empty),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such domain", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Not managed by SRMD (`not_managed`)", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn srmd_release(
    State(app): State<AppState>,
    auth: AdminAuth,
    Path(domain): Path<String>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Failure> {
    parse_empty(&body_bytes(body)?)?;
    srmd_action(app, auth, domain, SrmdManualAction::Release, "srmd.release").await
}

// ----- audit, events, stream ---------------------------------------------------------------

/// A JSON text column as a value; anything that does not parse is passed on as a string.
fn detail_value(text: String) -> Value {
    serde_json::from_str(&text).unwrap_or(Value::String(text))
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/audit",
    tag = "admin",
    security(("bearer" = [])),
    params(
        ("limit" = Option<i64>, Query, description = "1 to 200, default 50"),
        ("before" = Option<i64>, Query, description = "Only entries with a smaller id"),
    ),
    responses(
        (status = 200, description = "Audit entries, newest first", body = Vec<AuditEntry>),
        (status = 400, description = "Bad query", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn list_audit(
    State(app): State<AppState>,
    _auth: AdminAuth,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, Failure> {
    let (limit, before) = paging(&query)?;
    flushed(&app).await?;
    let entries = read(&app, move |conn| {
        conn.prepare_cached(
            "SELECT id, at, actor, action, target, detail FROM audit \
             WHERE (?1 IS NULL OR id < ?1) ORDER BY id DESC LIMIT ?2",
        )?
        .query_map(params![before, limit], |r| {
            Ok(AuditEntry {
                id: r.get(0)?,
                at: rfc3339(r.get(1)?),
                actor: r.get(2)?,
                action: r.get(3)?,
                target: r.get(4)?,
                detail: detail_value(r.get(5)?),
            })
        })?
        .collect::<rusqlite::Result<Vec<AuditEntry>>>()
    })
    .await?;
    Ok(json_status(StatusCode::OK, &entries))
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/events",
    tag = "admin",
    security(("bearer" = [])),
    params(
        ("limit" = Option<i64>, Query, description = "1 to 200, default 50"),
        ("before" = Option<i64>, Query, description = "Only events with a smaller id"),
        ("kind" = Option<String>, Query, description = "Only events of this kind, such as `node_dead`"),
        ("node" = Option<String>, Query, description = "Only events of this node"),
    ),
    responses(
        (status = 200, description = "Events, newest first", body = Vec<EventEntry>),
        (status = 400, description = "Bad query", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn list_events(
    State(app): State<AppState>,
    _auth: AdminAuth,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, Failure> {
    let (limit, before) = paging(&query)?;
    let kind = query.get("kind").cloned();
    let node = query.get("node").cloned();
    flushed(&app).await?;
    let events = read(&app, move |conn| {
        conn.prepare_cached(
            "SELECT id, at, kind, node, domain, detail FROM events \
             WHERE (?1 IS NULL OR id < ?1) AND (?2 IS NULL OR kind = ?2) \
             AND (?3 IS NULL OR node = ?3) ORDER BY id DESC LIMIT ?4",
        )?
        .query_map(params![before, kind, node, limit], |r| {
            Ok(EventEntry {
                id: r.get(0)?,
                at: rfc3339(r.get(1)?),
                kind: r.get(2)?,
                node: r.get(3)?,
                domain: r.get(4)?,
                detail: detail_value(r.get(5)?),
            })
        })?
        .collect::<rusqlite::Result<Vec<EventEntry>>>()
    })
    .await?;
    Ok(json_status(StatusCode::OK, &events))
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/stream",
    tag = "admin",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Server-sent events: one `event: <kind>` frame per pool event with `id` and a JSON `data` line, and a `: keepalive` comment every 15 seconds. The `id` counts the frames of this registrar run; there is no replay. The answer carries `X-Accel-Buffering: no` so the proxy passes frames at once.",
            body = String, content_type = "text/event-stream"),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn stream(State(app): State<AppState>, _auth: AdminAuth) -> Response {
    app.events.subscribe(app.actor.clone())
}

// ----- the panel's read paths --------------------------------------------------------------

const ROUNDS_SHOWN: i64 = 20;
const DAY_MS: i64 = 24 * 3600 * 1000;
/// The sensors in the order the panel lists them, under the names their votes are stored with.
const SENSORS: [&str; 3] = ["globalping", "cheburcheck", "red_flag"];

/// What of a node only the state owner knows.
struct NodeExtras {
    id: String,
    full_ip: String,
    owner_id: i64,
    last_heartbeat: Option<String>,
    last_report_at: Option<String>,
    port_filter: Option<String>,
    failed: Vec<String>,
}

/// Every node with a record (a dead or pruned one for its last 24 hours), as the admin sees it: the public document of
/// the snapshot plus the address, the owner, the heartbeat, the agent version and the failed
/// conditions.
async fn admin_nodes(app: &AppState) -> Result<Vec<AdminNode>, Failure> {
    let now = (app.clock)();
    let extras: Vec<NodeExtras> = app
        .actor
        .inspect(move |state| {
            state
                .nodes
                .values()
                .map(|n| NodeExtras {
                    id: n.id.as_str().to_owned(),
                    full_ip: n.ip.to_string(),
                    owner_id: n.owner.0,
                    last_heartbeat: (n.health.last_heartbeat.0 > 0)
                        .then(|| rfc3339(n.health.last_heartbeat.0)),
                    last_report_at: (n.health.last_report.0 > 0)
                        .then(|| rfc3339(n.health.last_report.0)),
                    port_filter: state.stats.get(&n.id).and_then(|s| s.port_filter.clone()),
                    failed: failed_conditions(&n.health, now, &state.settings)
                        .into_iter()
                        .filter_map(|c| match serde_json::to_value(c) {
                            Ok(Value::String(name)) => Some(name),
                            _ => None,
                        })
                        .collect(),
                })
                .collect()
        })
        .await
        .map_err(|_| actor_stopped())?;
    let owners: HashMap<i64, String> = read(app, |conn| {
        conn.prepare_cached("SELECT id, name FROM owners")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()
    })
    .await?;
    let versions = app.rollouts.agent_versions();
    let snapshot = app.snapshots.load();
    Ok(extras
        .into_iter()
        .filter_map(|e| {
            let node = snapshot.node_docs.get(&e.id)?.clone();
            Some(AdminNode {
                agent_version: NodeId::parse(&e.id)
                    .ok()
                    .and_then(|id| versions.get(&id).cloned()),
                owner_name: owners.get(&e.owner_id).cloned(),
                node,
                full_ip: e.full_ip,
                owner_id: e.owner_id,
                last_heartbeat: e.last_heartbeat,
                last_report_at: e.last_report_at,
                port_filter: e.port_filter,
                failed: e.failed,
            })
        })
        .collect())
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/nodes",
    operation_id = "admin_list_nodes",
    tag = "admin",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Every node with a record, with the whole address and the owner. A banned node has none; a dead or pruned one keeps it for 24 hours", body = Vec<AdminNode>),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn list_nodes(
    State(app): State<AppState>,
    _auth: AdminAuth,
) -> Result<Response, Failure> {
    Ok(json_status(StatusCode::OK, &admin_nodes(&app).await?))
}

/// The last rounds of `node`, newest first, with the evidence of each vote as it was stored.
fn admin_rounds(conn: &rusqlite::Connection, node: &str) -> rusqlite::Result<Vec<AdminRound>> {
    let rounds: Vec<(i64, i64, String)> = conn
        .prepare_cached(
            "SELECT id, at, verdict FROM sensor_rounds WHERE node = ?1 \
             ORDER BY at DESC, id DESC LIMIT ?2",
        )?
        .query_map(params![node, ROUNDS_SHOWN], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::with_capacity(rounds.len());
    for (round_id, at, verdict) in rounds {
        let Ok(verdict) = serde_json::from_value::<Verdict>(Value::String(verdict)) else {
            tracing::warn!(node, "a sensor round has a verdict the API does not know");
            continue;
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
                let data = serde_json::from_str::<Value>(&data).unwrap_or(Value::Null);
                let reason = (vote == Vote::Abstain)
                    .then(|| data.pointer("/vote/abstain")?.as_str().map(str::to_owned))
                    .flatten();
                Some(AdminVote {
                    sensor,
                    vote,
                    reason,
                    evidence: data.get("evidence").cloned().unwrap_or(Value::Null),
                })
            })
            .collect();
        out.push(AdminRound {
            at: rfc3339(at),
            verdict,
            votes,
        });
    }
    Ok(out)
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/nodes/{id}",
    operation_id = "admin_get_node",
    tag = "admin",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "The node id")),
    responses(
        (status = 200, description = "The node with its last 20 sensor rounds and their evidence", body = AdminNodeDetail),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such node", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn get_node(
    State(app): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
) -> Result<Response, Failure> {
    let node = node_of(&id)?;
    let found = admin_nodes(&app)
        .await?
        .into_iter()
        .find(|n| n.node.id == node.as_str())
        .ok_or_else(|| not_found("node"))?;
    flushed(&app).await?;
    let name = node.as_str().to_owned();
    let rounds = read(&app, move |conn| admin_rounds(conn, &name)).await?;
    Ok(json_status(
        StatusCode::OK,
        &AdminNodeDetail {
            node: found,
            rounds,
        },
    ))
}

/// What one sensor did lately, read from the votes of the rounds.
fn sensor_status(
    conn: &rusqlite::Connection,
    name: &str,
    configured: bool,
) -> rusqlite::Result<SensorStatus> {
    let last: Option<(String, String)> = conn
        .prepare_cached(
            "SELECT v.vote, v.data FROM sensor_votes v JOIN sensor_rounds r ON r.id = v.round_id \
             WHERE v.sensor = ?1 ORDER BY r.at DESC, r.id DESC LIMIT 1",
        )?
        .query_row([name], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let mut answered: Option<i64> = conn
        .prepare_cached(
            "SELECT MAX(r.at) FROM sensor_votes v JOIN sensor_rounds r ON r.id = v.round_id \
             WHERE v.sensor = ?1 AND v.vote <> 'abstain'",
        )?
        .query_row([name], |r| r.get(0))?;
    if name == "globalping" {
        // A measurement is an answer too, even when the round that asked for it abstained.
        let measured: Option<i64> = conn
            .prepare_cached("SELECT MAX(at) FROM gp_measurements")?
            .query_row([], |r| r.get(0))?;
        answered = answered.max(measured);
    }
    let last_vote = last
        .as_ref()
        .and_then(|(vote, _)| serde_json::from_value::<Vote>(Value::String(vote.clone())).ok());
    let last_reason = last.as_ref().and_then(|(vote, data)| {
        (vote == "abstain")
            .then(|| {
                serde_json::from_str::<Value>(data)
                    .ok()?
                    .pointer("/vote/abstain")?
                    .as_str()
                    .map(str::to_owned)
            })
            .flatten()
    });
    Ok(SensorStatus {
        name: name.to_owned(),
        configured,
        last_answer_at: answered.map(rfc3339),
        last_vote,
        last_reason,
    })
}

/// The DNS and event counters of the status page.
fn event_status(
    conn: &rusqlite::Connection,
    now: Ts,
) -> rusqlite::Result<(DnsStatus, StatusCounts)> {
    let day_ago = now.0 - DAY_MS;
    let latest = |kind: &str| -> rusqlite::Result<Option<(i64, String)>> {
        conn.prepare_cached(
            "SELECT at, detail FROM events WHERE kind = ?1 ORDER BY at DESC, id DESC LIMIT 1",
        )?
        .query_row([kind], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()
    };
    let since = |kind: &str, from: i64| -> rusqlite::Result<i64> {
        conn.prepare_cached("SELECT COUNT(*) FROM events WHERE kind = ?1 AND at >= ?2")?
            .query_row(params![kind, from], |r| r.get(0))
    };
    let applied = latest("dns_applied")?;
    let failed = latest("dns_failed")?;
    let dns = DnsStatus {
        token_set: false,
        last_applied_at: applied.map(|(at, _)| rfc3339(at)),
        last_error: failed.as_ref().and_then(|(_, detail)| {
            serde_json::from_str::<Value>(detail)
                .ok()?
                .get("error")?
                .as_str()
                .map(str::to_owned)
        }),
        last_failed_at: failed.map(|(at, _)| rfc3339(at)),
        applied_24h: since("dns_applied", day_ago)?,
        failed_24h: since("dns_failed", day_ago)?,
        unsynced_domains: 0,
    };
    // A row with `reverify` set is a failed attempt to lift a ban, not a new ban.
    let bans_7d = conn
        .prepare_cached("SELECT COUNT(*) FROM bans WHERE at >= ?1 AND reverify = 0")?
        .query_row([now.0 - 7 * DAY_MS], |r| r.get(0))?;
    let counts = StatusCounts {
        master_changes_24h: since("master_changed", day_ago)?,
        bans_7d,
    };
    Ok((dns, counts))
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/status",
    operation_id = "admin_status",
    tag = "admin",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "The sensors, DNS and the counters of the last days", body = AdminStatus),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn status(State(app): State<AppState>, _auth: AdminAuth) -> Result<Response, Failure> {
    let now = (app.clock)();
    flushed(&app).await?;
    let unsynced = app
        .actor
        .inspect(|state| {
            let synced = |domain: &String| {
                state
                    .dns
                    .get(domain)
                    .is_some_and(|d| d.applied.as_ref() == Some(&d.desired))
            };
            i64::try_from(state.settings.domains.iter().filter(|d| !synced(d)).count())
                .unwrap_or(i64::MAX)
        })
        .await
        .map_err(|_| actor_stopped())?;
    let store = app.store.clone();
    let (token_set, configured) = blocking(move || {
        (
            secrets::is_set(&store, "cloudflare_api_token"),
            [
                secrets::is_set(&store, "globalping_token"),
                true,
                secrets::is_set(&store, "redflag_api_key"),
            ],
        )
    })
    .await?;
    let (sensors, (mut dns, counts)) = read(&app, move |conn| {
        let sensors = SENSORS
            .iter()
            .zip(configured)
            .map(|(name, configured)| sensor_status(conn, name, configured))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((sensors, event_status(conn, now)?))
    })
    .await?;
    dns.token_set = token_set;
    dns.unsynced_domains = unsynced;
    Ok(json_status(
        StatusCode::OK,
        &AdminStatus {
            sensors,
            dns,
            counts,
        },
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/srmd",
    operation_id = "admin_srmd",
    tag = "admin",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "The domains SRMD keeps and how many the queue needs", body = SrmdOverview),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn srmd_overview(
    State(app): State<AppState>,
    _auth: AdminAuth,
) -> Result<Response, Failure> {
    let overview = app
        .actor
        .inspect(|state| {
            // The same count `rebalance` balances the domains for.
            let queue = state
                .nodes
                .values()
                .filter(|n| n.state == NodeState::Healthy)
                .count();
            let (folded, active): (Vec<String>, Vec<String>) = state
                .settings
                .domains
                .iter()
                .cloned()
                .partition(|d| state.srmd.cnames.contains_key(d));
            SrmdOverview {
                enabled: state.settings.srmd.enabled,
                needed: u32::try_from(needed_domains(queue, &state.settings)).unwrap_or(u32::MAX),
                active,
                folded,
                created: state.srmd.created.iter().cloned().collect(),
                cnames: state.srmd.cnames.clone(),
                clients: state.srmd.domain_clients.clone(),
            }
        })
        .await
        .map_err(|_| actor_stopped())?;
    Ok(json_status(StatusCode::OK, &overview))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/dns/push",
    operation_id = "admin_dns_push",
    tag = "admin",
    security(("bearer" = [])),
    request_body(content = EmptyRequest, description = "Empty, or `{}`"),
    responses(
        (status = 202, description = "Every domain is pushed to DNS again on the next ticks; one that is being pushed now is left alone", body = Empty),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn dns_push(
    State(app): State<AppState>,
    auth: AdminAuth,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Failure> {
    parse_empty(&body_bytes(body)?)?;
    app.actor
        .send(Command::DnsPushAll)
        .await
        .map_err(|_| actor_stopped())?;
    record(&app, &auth, "dns.push", None, json!({}));
    Ok(json_status(StatusCode::ACCEPTED, &json!({})))
}
