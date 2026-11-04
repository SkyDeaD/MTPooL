mod common;

use std::collections::BTreeSet;

use axum::http::StatusCode;
use common::http::{Env, admin_token, assert_problem, call, count, env, get, node_at, pool, post};
use common::node_id;
use mtpool_core::{NodeState, RolloutSettings};
use mtpool_registrar::auth::tokens::{self, NewToken, TokenKind};
use serde_json::{Value, json};

const NODE: &str = "alpha-00001";

fn node_token(env: &Env, name: &str) -> String {
    tokens::issue(
        &env.store,
        NewToken {
            kind: TokenKind::Node,
            owner: None,
            node: Some(node_id(name)),
            expires_at: None,
            scope: String::new(),
        },
    )
    .expect("token")
}

fn one_node() -> Env {
    env(pool(vec![node_at(
        NODE,
        [95, 216, 0, 1],
        1,
        NodeState::Healthy,
    )]))
}

async fn tick(env: &Env) {
    let nodes = env
        .actor
        .inspect(|s| s.nodes.values().cloned().collect::<Vec<_>>())
        .await
        .unwrap();
    env.rollouts.tick(
        &nodes,
        &BTreeSet::new(),
        mtpool_core::Ts(env.clock.load(std::sync::atomic::Ordering::SeqCst)),
    );
}

fn start(env: &Env) {
    let now = mtpool_core::Ts(env.clock.load(std::sync::atomic::Ordering::SeqCst));
    assert!(
        env.rollouts
            .offer_release("0.2.0", now, RolloutSettings::default())
    );
}

async fn poll(env: &Env, token: &str, version: &str) -> common::http::Resp {
    post(
        env,
        "/api/v1/agent/update",
        token,
        Some(json!({ "version": version })),
    )
    .await
}

async fn status(env: &Env, token: &str, body: Value) -> common::http::Resp {
    post(env, "/api/v1/agent/update-status", token, Some(body)).await
}

#[tokio::test]
async fn poll_without_a_rollout_offers_nothing() {
    let env = one_node();
    let token = node_token(&env, NODE);
    let resp = poll(&env, &token, "0.1.0").await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.json(), json!({}));
}

#[tokio::test]
async fn poll_needs_a_node_token_a_known_node_and_a_version() {
    let env = one_node();
    let resp = post(
        &env,
        "/api/v1/agent/update",
        "mtp_node_nope",
        Some(json!({"version": "0.1.0"})),
    )
    .await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
    let ghost = node_token(&env, "ghost-00001");
    let resp = poll(&env, &ghost, "0.1.0").await;
    assert_problem(&resp, StatusCode::GONE, "unknown_node");
    let token = node_token(&env, NODE);
    let resp = poll(&env, &token, "nonsense").await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    let resp = post(
        &env,
        "/api/v1/agent/update",
        &token,
        Some(json!({"version": "0.1.0", "x": 1})),
    )
    .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_canary_is_told_to_update_and_reports_through_the_admin_api() {
    let env = one_node();
    let token = node_token(&env, NODE);
    let admin = admin_token(&env);
    start(&env);
    // The first poll makes the node known to the rollout; a tick picks the canary.
    assert_eq!(poll(&env, &token, "0.1.0").await.json(), json!({}));
    tick(&env).await;
    assert_eq!(
        poll(&env, &token, "0.1.0").await.json(),
        json!({"version": "0.2.0"})
    );

    let resp = status(
        &env,
        &token,
        json!({"version": "0.2.0", "outcome": "started"}),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    let view = get(&env, "/api/v1/admin/rollouts/1", &admin).await.json();
    assert_eq!(view["phase"], "canary");
    assert_eq!(view["canary"], NODE);
    assert_eq!(view["nodes"][0]["stage"], "started");
    assert_eq!(view["counts"]["started"], 1);
    assert_eq!(view["can_resume"], false);
    assert_eq!(view["can_cancel"], true);

    status(
        &env,
        &token,
        json!({"version": "0.2.0", "outcome": "updated"}),
    )
    .await;
    tick(&env).await;
    let view = get(&env, "/api/v1/admin/rollouts/1", &admin).await.json();
    assert_eq!(view["phase"], "observing");
    assert!(view["observing_since"].is_string());
}

#[tokio::test]
async fn a_failed_update_pauses_with_a_reason_and_resume_and_cancel_are_audited() {
    let env = one_node();
    let token = node_token(&env, NODE);
    let admin = admin_token(&env);
    start(&env);
    poll(&env, &token, "0.1.0").await;
    tick(&env).await;
    status(
        &env,
        &token,
        json!({"version": "0.2.0", "outcome": "failed", "code": "bad_signature"}),
    )
    .await;

    let list = get(&env, "/api/v1/admin/rollouts", &admin).await.json();
    assert_eq!(list.as_array().unwrap().len(), 1);
    let view = &list[0];
    assert_eq!(view["phase"], "paused");
    assert_eq!(view["note"]["code"], "bad_signature");
    assert!(!view["note"]["text"].as_str().unwrap().is_empty());
    assert_eq!(view["can_resume"], true);

    // A failed report needs a plain code.
    let resp = status(
        &env,
        &token,
        json!({"version": "0.2.0", "outcome": "failed"}),
    )
    .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    let resp = status(
        &env,
        &token,
        json!({"version": "0.2.0", "outcome": "failed", "code": "Bad Code"}),
    )
    .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);

    let resp = post(&env, "/api/v1/admin/rollouts/1/resume", &admin, None).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.json()["phase"], "canary");
    assert!(resp.json()["note"].is_null());
    let again = post(&env, "/api/v1/admin/rollouts/1/resume", &admin, None).await;
    assert_problem(&again, StatusCode::CONFLICT, "not_paused");

    let resp = post(
        &env,
        "/api/v1/admin/rollouts/1/cancel",
        &admin,
        Some(json!({})),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.json()["phase"], "cancelled");
    let again = post(&env, "/api/v1/admin/rollouts/1/cancel", &admin, None).await;
    assert_problem(&again, StatusCode::CONFLICT, "rollout_over");

    env.store.flush().unwrap();
    assert_eq!(
        count(
            &env,
            "SELECT count(*) FROM audit WHERE action = 'rollout.resume' AND target = '1'"
        ),
        1
    );
    assert_eq!(
        count(
            &env,
            "SELECT count(*) FROM audit WHERE action = 'rollout.cancel' AND target = '1'"
        ),
        1
    );
    assert_eq!(count(&env, "SELECT count(*) FROM rollouts"), 1);
}

#[tokio::test]
async fn the_admin_routes_need_the_admin_token_and_a_rollout_that_exists() {
    let env = one_node();
    let node = node_token(&env, NODE);
    let admin = admin_token(&env);
    assert_eq!(
        get(&env, "/api/v1/admin/rollouts", &node).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&env, "GET", "/api/v1/admin/rollouts", "x", None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get(&env, "/api/v1/admin/rollouts", &admin).await.json(),
        json!([])
    );
    assert_problem(
        &get(&env, "/api/v1/admin/rollouts/7", &admin).await,
        StatusCode::NOT_FOUND,
        "not_found",
    );
    assert_problem(
        &get(&env, "/api/v1/admin/rollouts/abc", &admin).await,
        StatusCode::NOT_FOUND,
        "not_found",
    );
    assert_problem(
        &post(&env, "/api/v1/admin/rollouts/7/resume", &admin, None).await,
        StatusCode::NOT_FOUND,
        "not_found",
    );
}

#[tokio::test]
async fn with_no_canary_candidate_the_rollout_waits_and_says_why() {
    let env = env(pool(vec![node_at(
        NODE,
        [95, 216, 0, 1],
        1,
        NodeState::Quarantine,
    )]));
    let token = node_token(&env, NODE);
    let admin = admin_token(&env);
    start(&env);
    poll(&env, &token, "0.1.0").await;
    tick(&env).await;
    let view = get(&env, "/api/v1/admin/rollouts/1", &admin).await.json();
    assert_eq!(view["phase"], "canary");
    assert_eq!(view["note"]["code"], "no_canary_candidate");
    assert!(view["canary"].is_null());
}
