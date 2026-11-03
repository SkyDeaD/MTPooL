//! `GET /metrics`: a metrics token only, the pool in Prometheus text format.

mod common;

use axum::http::StatusCode;
use common::http::*;
use mtpool_core::NodeState;
use mtpool_registrar::auth::tokens::TokenKind;

#[tokio::test]
async fn metrics_requires_token() {
    let env = env(pool(vec![]));
    let resp = send(&env, request("GET", "/metrics", None, None)).await;
    assert_problem(&resp, StatusCode::UNAUTHORIZED, "unauthorized");
    let resp = get(&env, "/metrics", "mtp_metrics_nosuchtoken").await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED, "{}", resp.body);
    // Tokens of the other kinds do not open it.
    let admin = admin_token(&env);
    assert_eq!(
        get(&env, "/metrics", &admin).await.status,
        StatusCode::FORBIDDEN
    );
    let owner = owner(&env, "alice", 5);
    let bot = bot_token(&env, owner);
    assert_eq!(
        get(&env, "/metrics", &bot).await.status,
        StatusCode::FORBIDDEN
    );
    // And it is not under the API prefix.
    let metrics = token(&env, TokenKind::Metrics, None);
    assert_eq!(
        get(&env, "/api/v1/metrics", &metrics).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn metrics_contains_node_state_counts() {
    let env = env(pool(vec![
        node_at("alpha-00001", [45, 33, 1, 5], 1, NodeState::Healthy),
        node_at("alpha-00002", [45, 33, 1, 6], 1, NodeState::Healthy),
        node_at("alpha-00003", [45, 33, 1, 7], 1, NodeState::Quarantine),
        node_at("alpha-00004", [45, 33, 1, 8], 1, NodeState::Dead),
    ]));
    let metrics = token(&env, TokenKind::Metrics, None);
    let resp = get(&env, "/metrics", &metrics).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.body);
    assert_eq!(
        resp.content_type(),
        "text/plain; version=0.0.4; charset=utf-8"
    );
    for line in [
        "mtpool_nodes{state=\"healthy\"} 2",
        "mtpool_nodes{state=\"quarantine\"} 1",
        "mtpool_nodes{state=\"dead\"} 1",
        "mtpool_nodes{state=\"pending\"} 0",
        "mtpool_bans_total 0",
        "mtpool_store_healthy 1",
        "mtpool_webhook_deliveries{status=\"pending\"} 0",
    ] {
        assert!(
            resp.body.lines().any(|l| l == line),
            "missing {line:?} in\n{}",
            resp.body
        );
    }
    assert!(resp.body.contains("mtpool_build_info{version=\""));
}
