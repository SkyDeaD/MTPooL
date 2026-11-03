//! `GET /metrics`: the pool in Prometheus text format 0.0.4.
//!
//! The route sits outside the API prefix, takes a metrics token (`MetricsAuth`) and is not
//! behind the public request limit. The figures of the snapshot (nodes by state, masters by
//! domain) come from the last published snapshot, so a scrape never waits for the state owner;
//! the ban count and the delivery counts are read from the database.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use mtpool_protocol::problem::Problem;
use mtpool_protocol::public::NodeState;
use rusqlite::Connection;
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::api::ApiError;
use crate::api::public::read;
use crate::auth::middleware::MetricsAuth;
use crate::server::AppState;
use crate::snapshot::Snapshot;
use crate::store::WriteHealth;

#[cfg(test)]
mod tests;

/// The content type of the text format, version 0.0.4.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Every state a node can be in, in the order they are listed.
const STATES: [(NodeState, &str); 6] = [
    (NodeState::Pending, "pending"),
    (NodeState::Healthy, "healthy"),
    (NodeState::Quarantine, "quarantine"),
    (NodeState::Banned, "banned"),
    (NodeState::Dead, "dead"),
    (NodeState::Pruned, "pruned"),
];

/// Every status a delivery can have.
const DELIVERY_STATUSES: [&str; 3] = ["pending", "delivered", "failed"];

/// The figures that are not in the snapshot; the handler reads them from the database and the
/// store.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extras {
    /// Bans ever recorded.
    pub bans_total: u64,
    pub store_healthy: bool,
    /// Webhook deliveries by status.
    pub deliveries: BTreeMap<String, u64>,
}

/// The text of the endpoint.
pub fn render_metrics(snapshot: &Snapshot, extras: &Extras) -> String {
    let mut out = String::new();

    family(&mut out, "mtpool_nodes", "gauge", "Nodes by state.");
    for (state, name) in STATES {
        let count = snapshot
            .node_docs
            .values()
            .filter(|n| n.state == state)
            .count();
        let _ = writeln!(out, "mtpool_nodes{{state=\"{name}\"}} {count}");
    }

    family(
        &mut out,
        "mtpool_masters",
        "gauge",
        "Masters assigned to each configured domain.",
    );
    for (domain, count) in &snapshot.master_counts {
        let _ = writeln!(
            out,
            "mtpool_masters{{domain=\"{}\"}} {count}",
            label_value(domain)
        );
    }

    family(
        &mut out,
        "mtpool_bans_total",
        "counter",
        "Bans recorded since the database was created.",
    );
    let _ = writeln!(out, "mtpool_bans_total {}", extras.bans_total);

    family(
        &mut out,
        "mtpool_store_healthy",
        "gauge",
        "1 while the last database write succeeded, 0 after a failed one.",
    );
    let _ = writeln!(
        out,
        "mtpool_store_healthy {}",
        u8::from(extras.store_healthy)
    );

    family(
        &mut out,
        "mtpool_webhook_deliveries",
        "gauge",
        "Webhook deliveries by status.",
    );
    for status in DELIVERY_STATUSES {
        let count = extras.deliveries.get(status).copied().unwrap_or(0);
        let _ = writeln!(
            out,
            "mtpool_webhook_deliveries{{status=\"{status}\"}} {count}"
        );
    }

    family(
        &mut out,
        "mtpool_build_info",
        "gauge",
        "The version of the registrar; the value is always 1.",
    );
    let _ = writeln!(
        out,
        "mtpool_build_info{{version=\"{}\"}} 1",
        label_value(env!("CARGO_PKG_VERSION"))
    );
    out
}

fn family(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

/// A label value as the text format wants it: backslash, quote and line feed escaped.
fn label_value(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

#[derive(OpenApi)]
#[openapi(
    tags((name = "ops", description = "Monitoring; needs a metrics token (`mtp_metrics_...`)")),
    components(schemas(Problem))
)]
struct MetricsDoc;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::with_openapi(MetricsDoc::openapi()).routes(routes!(get_metrics))
}

#[utoipa::path(
    get,
    path = "/metrics",
    tag = "ops",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "The pool in Prometheus text format 0.0.4", body = String, content_type = "text/plain; version=0.0.4; charset=utf-8"),
        (status = 401, description = "No valid metrics token", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A token of another kind", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn get_metrics(
    State(app): State<AppState>,
    _auth: MetricsAuth,
) -> Result<Response, ApiError> {
    let (bans_total, deliveries) = read(&app, counts).await?;
    let extras = Extras {
        bans_total,
        store_healthy: matches!(app.store.health(), WriteHealth::Ok),
        deliveries,
    };
    let body = render_metrics(&app.snapshots.load(), &extras);
    Ok((StatusCode::OK, [(header::CONTENT_TYPE, CONTENT_TYPE)], body).into_response())
}

fn counts(conn: &Connection) -> rusqlite::Result<(u64, BTreeMap<String, u64>)> {
    let bans: i64 = conn.query_row("SELECT COUNT(*) FROM bans", [], |r| r.get(0))?;
    let mut statement =
        conn.prepare("SELECT status, COUNT(*) FROM webhook_deliveries GROUP BY status")?;
    let deliveries = statement
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .map(|row| row.map(|(status, n)| (status, u64::try_from(n).unwrap_or(0))))
        .collect::<rusqlite::Result<_>>()?;
    Ok((u64::try_from(bans).unwrap_or(0), deliveries))
}
