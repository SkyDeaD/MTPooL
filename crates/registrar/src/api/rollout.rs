//! The admin routes of agent rollouts under `/api/v1/admin/rollouts`.
//!
//! | route                                | answer                                         |
//! |--------------------------------------|------------------------------------------------|
//! | `GET /admin/rollouts`                | 200, newest first                              |
//! | `GET /admin/rollouts/{id}`           | 200, 404                                       |
//! | `POST /admin/rollouts/{id}/resume`   | 200, 404, 409 `not_paused`                     |
//! | `POST /admin/rollouts/{id}/cancel`   | 200, 404, 409 `rollout_over`                   |
//!
//! Resume and cancel are the admin's decisions and are written to the audit log.

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use mtpool_core::rollout::{Phase, Resume, Rollout, Stage};
use mtpool_core::{NodeRecord, Ts};
use mtpool_protocol::admin::{
    RolloutCounts, RolloutNode, RolloutNote, RolloutTimings, RolloutView,
};
use mtpool_protocol::agent::EmptyRequest;
use mtpool_protocol::problem::Problem;
use mtpool_protocol::rfc3339;
use serde_json::json;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::ApiError;
use super::agent::{Failure, actor_stopped, blocking, body_bytes, parse_empty};
use super::util::{json_status, numeric_id};
use crate::audit;
use crate::auth::middleware::AdminAuth;
use crate::server::AppState;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_rollouts))
        .routes(routes!(get_rollout))
        .routes(routes!(resume_rollout))
        .routes(routes!(cancel_rollout))
}

fn at(ts: Ts) -> String {
    rfc3339(ts.0)
}

/// The admin's view of a rollout.
pub fn view(r: &Rollout) -> RolloutView {
    let mut counts = RolloutCounts::default();
    let mut nodes = Vec::with_capacity(r.members.len());
    for (id, m) in &r.members {
        let (stage, when, code) = match &m.stage {
            Stage::Idle => {
                counts.idle += 1;
                ("idle", None, None)
            }
            Stage::Offered { at } => {
                counts.offered += 1;
                ("offered", Some(*at), None)
            }
            Stage::Started { at } => {
                counts.started += 1;
                ("started", Some(*at), None)
            }
            Stage::Updated { at } => {
                counts.updated += 1;
                ("updated", Some(*at), None)
            }
            Stage::Failed { at, code } => {
                counts.failed += 1;
                ("failed", Some(*at), Some(code.clone()))
            }
            Stage::Missed { at } => {
                counts.missed += 1;
                ("missed", Some(*at), None)
            }
        };
        nodes.push(RolloutNode {
            node_id: id.as_str().to_owned(),
            version: m.version.clone(),
            stage: stage.to_owned(),
            at: when.map(at),
            code,
        });
    }
    let (phase, resume_to, observing_since, next_batch_at, changed_at) = match r.phase {
        Phase::Canary => ("canary", None, None, None, None),
        Phase::Observing { since } => ("observing", None, Some(at(since)), None, None),
        Phase::Batches { next_at } => ("batches", None, None, Some(at(next_at)), None),
        Phase::Paused { resume, at: when } => {
            let to = match resume {
                Resume::Canary => "canary",
                Resume::Observing => "observing",
                Resume::Batches => "batches",
            };
            ("paused", Some(to.to_owned()), None, None, Some(at(when)))
        }
        Phase::Done { at: when } => ("done", None, None, None, Some(at(when))),
        Phase::Cancelled { at: when } => ("cancelled", None, None, None, Some(at(when))),
    };
    let t = r.timings;
    RolloutView {
        id: r.id,
        version: r.version.clone(),
        created_at: at(r.created_at),
        phase: phase.to_owned(),
        resume_to,
        observing_since,
        next_batch_at,
        changed_at,
        note: r.note.as_ref().map(|n| RolloutNote {
            code: n.code.clone(),
            text: n.text.clone(),
        }),
        canary: r.canary.as_ref().map(|c| c.as_str().to_owned()),
        can_resume: matches!(r.phase, Phase::Paused { .. }),
        can_cancel: r.is_open(),
        timings: RolloutTimings {
            observe_sec: t.observe_sec,
            batch_interval_sec: t.batch_interval_sec,
            batch_percent: t.batch_percent,
            return_sec: t.return_sec,
            offer_wait_sec: t.offer_wait_sec,
        },
        counts,
        nodes,
    }
}

fn storage_failed(e: &crate::store::StoreError) -> Failure {
    tracing::error!(error = %e, "admin API: cannot read the rollouts");
    ApiError::internal().into()
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/rollouts",
    tag = "admin",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Every rollout, newest first", body = Vec<RolloutView>),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn list_rollouts(
    State(app): State<AppState>,
    _auth: AdminAuth,
) -> Result<Response, Failure> {
    let svc = app.rollouts.clone();
    let all = blocking(move || svc.list())
        .await?
        .map_err(|e| storage_failed(&e))?;
    let views: Vec<RolloutView> = all.iter().map(view).collect();
    Ok(json_status(StatusCode::OK, &views))
}

async fn find(app: &AppState, id: &str) -> Result<Rollout, Failure> {
    let id = numeric_id(id, "rollout")?;
    let svc = app.rollouts.clone();
    blocking(move || svc.get(id))
        .await?
        .map_err(|e| storage_failed(&e))?
        .ok_or_else(|| ApiError::not_found("No such rollout.").into())
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/rollouts/{id}",
    tag = "admin",
    security(("bearer" = [])),
    params(("id" = i64, Path, description = "The rollout id")),
    responses(
        (status = 200, description = "The rollout with the stage of every node", body = RolloutView),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such rollout", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn get_rollout(
    State(app): State<AppState>,
    _auth: AdminAuth,
    Path(id): Path<String>,
) -> Result<Response, Failure> {
    let r = find(&app, &id).await?;
    Ok(json_status(StatusCode::OK, &view(&r)))
}

async fn nodes_now(app: &AppState) -> Result<Vec<NodeRecord>, Failure> {
    app.actor
        .inspect(|state| state.nodes.values().cloned().collect::<Vec<_>>())
        .await
        .map_err(|_| actor_stopped())
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/rollouts/{id}/resume",
    tag = "admin",
    security(("bearer" = [])),
    params(("id" = i64, Path, description = "The rollout id")),
    request_body(content = EmptyRequest, description = "Empty, or `{}`"),
    responses(
        (status = 200, description = "The rollout goes on from where it stopped", body = RolloutView),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such rollout", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The rollout is not paused (`not_paused`)", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn resume_rollout(
    State(app): State<AppState>,
    auth: AdminAuth,
    Path(id): Path<String>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Failure> {
    parse_empty(&body_bytes(body)?)?;
    let id = numeric_id(&id, "rollout")?;
    let nodes = nodes_now(&app).await?;
    let now = (app.clock)();
    let svc = app.rollouts.clone();
    let done = blocking(move || {
        let done = svc.resume(id, &nodes, now);
        if done == Some(true) {
            svc.flush();
        }
        done
    })
    .await?;
    answer(&app, &auth, "rollout.resume", id, done, "not_paused").await
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/rollouts/{id}/cancel",
    tag = "admin",
    security(("bearer" = [])),
    params(("id" = i64, Path, description = "The rollout id")),
    request_body(content = EmptyRequest, description = "Empty, or `{}`"),
    responses(
        (status = 200, description = "The rollout is cancelled; nodes keep the version they run", body = RolloutView),
        (status = 401, description = "No valid admin credential", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such rollout", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The rollout is over already (`rollout_over`)", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn cancel_rollout(
    State(app): State<AppState>,
    auth: AdminAuth,
    Path(id): Path<String>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Failure> {
    parse_empty(&body_bytes(body)?)?;
    let id = numeric_id(&id, "rollout")?;
    let now = (app.clock)();
    let svc = app.rollouts.clone();
    let done = blocking(move || {
        let done = svc.cancel(id, now);
        if done == Some(true) {
            svc.flush();
        }
        done
    })
    .await?;
    answer(&app, &auth, "rollout.cancel", id, done, "rollout_over").await
}

/// `done`: `None` no such rollout, `Some(false)` it was not in a state for the action.
async fn answer(
    app: &AppState,
    auth: &AdminAuth,
    action: &str,
    id: i64,
    done: Option<bool>,
    conflict_code: &str,
) -> Result<Response, Failure> {
    match done {
        None => Err(ApiError::not_found("No such rollout.").into()),
        Some(false) => Err(ApiError::new(
            StatusCode::CONFLICT,
            conflict_code,
            "The rollout is not in a state for this action.",
            if conflict_code == "not_paused" {
                "Only a paused rollout can be continued."
            } else {
                "The rollout is over already."
            },
        )
        .into()),
        Some(true) => {
            audit::record(
                &app.store,
                (app.clock)(),
                &audit::actor_of(auth),
                action,
                Some(&id.to_string()),
                json!({}),
            );
            let r = find(app, &id.to_string()).await?;
            Ok(json_status(StatusCode::OK, &view(&r)))
        }
    }
}
