//! The bot API: an owner's own nodes, retire and recheck, webhook registration.

mod common;

use axum::http::StatusCode;
use common::http::*;
use mtpool_core::{NodeState, Ts};
use mtpool_registrar::auth::tokens::TokenKind;
use serde_json::json;

fn alpha() -> mtpool_core::NodeRecord {
    node_at("alpha-00001", [45, 33, 1, 5], 1, NodeState::Healthy)
}

fn beta() -> mtpool_core::NodeRecord {
    node_at("beta-00002", [45, 33, 1, 6], 2, NodeState::Healthy)
}

/// Two owners with a bot token each; owner 1 has alpha, owner 2 has beta.
fn two_owners() -> (Env, String, String) {
    let env = env(pool(vec![alpha(), beta()]));
    let a = owner(&env, "owner-a", 5);
    let b = owner(&env, "owner-b", 5);
    assert_eq!((a.0, b.0), (1, 2));
    let ta = bot_token(&env, a);
    let tb = bot_token(&env, b);
    (env, ta, tb)
}

#[tokio::test]
async fn me_nodes_full_ip_only_own() {
    let (env, ta, _tb) = two_owners();

    let me = get(&env, "/api/v1/me", &ta).await;
    assert_eq!(me.status, StatusCode::OK, "{}", me.body);
    let me = me.json();
    assert_eq!(me["owner"]["id"], 1, "{me}");
    assert_eq!(me["owner"]["name"], "owner-a", "{me}");
    assert_eq!(me["owner"]["node_limit"], 5, "{me}");
    assert!(
        me["token"]["prefix"]
            .as_str()
            .unwrap()
            .starts_with("mtp_bot_"),
        "{me}"
    );
    assert!(me["token"]["id"].is_number(), "{me}");
    assert!(me["token"]["created_at"].is_string(), "{me}");
    assert!(
        !me.to_string().contains(&ta),
        "the token value must not be echoed"
    );

    let nodes = get(&env, "/api/v1/me/nodes", &ta).await;
    assert_eq!(nodes.status, StatusCode::OK, "{}", nodes.body);
    let nodes = nodes.json();
    let nodes = nodes.as_array().expect("an array");
    assert_eq!(nodes.len(), 1, "only the owner's nodes: {nodes:?}");
    assert_eq!(nodes[0]["id"], "alpha-00001");
    assert_eq!(nodes[0]["ip"], ALPHA_IP, "the full address, not a mask");
    assert_eq!(nodes[0]["state"], "healthy");
    assert!(nodes[0].get("last_round").is_some(), "{nodes:?}");

    // No token and the wrong kind of token.
    let anonymous = send(&env, request("GET", "/api/v1/me/nodes", None, None)).await;
    assert_problem(&anonymous, StatusCode::UNAUTHORIZED, "unauthorized");
    let admin = admin_token(&env);
    let wrong = get(&env, "/api/v1/me/nodes", &admin).await;
    assert!(
        wrong.status == StatusCode::FORBIDDEN || wrong.status == StatusCode::UNAUTHORIZED,
        "{}",
        wrong.body
    );
}

#[tokio::test]
async fn other_owner_node_returns_404() {
    let (env, ta, _tb) = two_owners();

    let retire = post(&env, "/api/v1/me/nodes/beta-00002/retire", &ta, None).await;
    assert_problem(&retire, StatusCode::NOT_FOUND, "not_found");
    let recheck = post(&env, "/api/v1/me/nodes/beta-00002/recheck", &ta, None).await;
    assert_problem(&recheck, StatusCode::NOT_FOUND, "not_found");
    // A node that does not exist looks exactly the same.
    let missing = post(&env, "/api/v1/me/nodes/nobody-00009/retire", &ta, None).await;
    assert_problem(&missing, StatusCode::NOT_FOUND, "not_found");
    assert_eq!(missing.json()["title"], retire.json()["title"]);

    assert_eq!(
        node_state(&env, "beta-00002").await,
        Some(NodeState::Healthy)
    );
}

#[tokio::test]
async fn retire_own_node() {
    let (env, ta, _tb) = two_owners();

    let resp = post(&env, "/api/v1/me/nodes/alpha-00001/retire", &ta, None).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.body);
    assert_eq!(node_state(&env, "alpha-00001").await, Some(NodeState::Dead));
    assert_eq!(
        node_state(&env, "beta-00002").await,
        Some(NodeState::Healthy)
    );
}

#[tokio::test]
async fn recheck_rate_limited() {
    // The node reported a heartbeat ahead of the clock, so it stays alive (a dead server is not
    // rechecked) while the test moves the clock past the allowance.
    let mut quarantined = node_at("alpha-00001", [45, 33, 1, 5], 1, NodeState::Quarantine);
    quarantined.health.last_heartbeat = Ts(NOW + 20 * 60 * 1000);
    let env = env(pool(vec![
        quarantined,
        node_at("gamma-00003", [45, 33, 1, 7], 1, NodeState::Healthy),
    ]));
    let a = owner(&env, "owner-a", 5);
    let ta = bot_token(&env, a);
    let path = "/api/v1/me/nodes/alpha-00001/recheck";

    let first = post(&env, path, &ta, None).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);

    let second = post(&env, path, &ta, None).await;
    assert_problem(&second, StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    let wait: i64 = second.retry_after().expect("Retry-After").parse().unwrap();
    assert!((1..=600).contains(&wait), "{wait}");

    env.advance(10 * 60 * 1000 + 1000);
    let third = post(&env, path, &ta, None).await;
    assert_eq!(third.status, StatusCode::OK, "{}", third.body);

    // A healthy node has nothing to recheck.
    let healthy = post(&env, "/api/v1/me/nodes/gamma-00003/recheck", &ta, None).await;
    assert_problem(&healthy, StatusCode::CONFLICT, "not_recheckable");
}

#[tokio::test]
async fn webhook_url_must_be_https_public() {
    let (env, ta, _tb) = two_owners();
    env.resolver.set("hooks.example.com", &["93.184.216.34"]);
    env.resolver.set("internal.example.com", &["10.0.0.5"]);
    env.resolver.set("loop.example.com", &["127.0.0.1"]);
    env.resolver.set("link.example.com", &["169.254.169.254"]);
    env.resolver.set("cgnat.example.com", &["100.64.0.9"]);
    env.resolver.set("six.example.com", &["fd00::1"]);
    env.resolver
        .set("mixed.example.com", &["93.184.216.34", "192.168.1.10"]);

    let create = |url: String| {
        let env = &env;
        let ta = ta.clone();
        async move {
            post(
                env,
                "/api/v1/me/webhooks",
                &ta,
                Some(json!({ "url": url, "events": ["node.dead"] })),
            )
            .await
        }
    };

    for url in [
        "http://hooks.example.com/hook",
        "https://93.184.216.34/hook",
        "https://[2001:db8::1]/hook",
        "https://localhost/hook",
        "https://unknown.example.com/hook",
        "https://internal.example.com/hook",
        "https://loop.example.com/hook",
        "https://link.example.com/hook",
        "https://cgnat.example.com/hook",
        "https://six.example.com/hook",
        "https://mixed.example.com/hook",
        "ftp://hooks.example.com/hook",
        "not a url",
    ] {
        let resp = create(url.to_owned()).await;
        assert_problem(&resp, StatusCode::BAD_REQUEST, "webhook_url");
    }
    let long = format!("https://hooks.example.com/{}", "a".repeat(2100));
    assert_problem(&create(long).await, StatusCode::BAD_REQUEST, "webhook_url");

    let unknown_event = post(
        &env,
        "/api/v1/me/webhooks",
        &ta,
        Some(json!({ "url": "https://hooks.example.com/hook", "events": ["no.such.event"] })),
    )
    .await;
    assert_problem(&unknown_event, StatusCode::BAD_REQUEST, "bad_request");

    assert_eq!(count(&env, "SELECT count(*) FROM webhooks"), 0);

    let ok = create("https://hooks.example.com/hook".to_owned()).await;
    assert_eq!(ok.status, StatusCode::CREATED, "{}", ok.body);
    let ok = ok.json();
    assert_eq!(ok["url"], "https://hooks.example.com/hook");
    assert_eq!(ok["scope"], "owner");
    assert!(ok["secret"].as_str().is_some_and(|s| s.len() >= 32), "{ok}");
    assert_eq!(count(&env, "SELECT count(*) FROM webhooks"), 1);

    // A listing never shows the secret again.
    let list = get(&env, "/api/v1/me/webhooks", &ta).await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.body);
    assert_eq!(list.json().as_array().unwrap().len(), 1);
    assert!(!list.body.contains(ok["secret"].as_str().unwrap()));

    // A pool-wide webhook needs the pool:read scope on the token.
    let pool_wide = post(
        &env,
        "/api/v1/me/webhooks",
        &ta,
        Some(json!({
            "url": "https://hooks.example.com/pool",
            "events": ["node.dead"],
            "scope": "pool"
        })),
    )
    .await;
    assert_problem(&pool_wide, StatusCode::FORBIDDEN, "forbidden");
    assert_eq!(count(&env, "SELECT count(*) FROM webhooks"), 1);
}

#[tokio::test]
async fn webhook_test_and_deliveries_endpoints() {
    let (env, ta, tb) = two_owners();
    env.resolver.set("hooks.example.com", &["93.184.216.34"]);

    let created = post(
        &env,
        "/api/v1/me/webhooks",
        &ta,
        Some(json!({ "url": "https://hooks.example.com/hook", "events": ["node.dead"] })),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let id = created.json()["id"].as_i64().unwrap();

    let tested = post(&env, &format!("/api/v1/me/webhooks/{id}/test"), &ta, None).await;
    assert_eq!(tested.status, StatusCode::ACCEPTED, "{}", tested.body);
    let tested = tested.json();
    assert_eq!(tested["event"], "webhook.test");
    assert_eq!(tested["status"], "pending");
    let first_event = tested["event_id"].as_str().unwrap().to_owned();
    assert!(!first_event.is_empty());

    env.advance(1000);
    let again = post(&env, &format!("/api/v1/me/webhooks/{id}/test"), &ta, None).await;
    assert_eq!(again.status, StatusCode::ACCEPTED, "{}", again.body);
    assert_ne!(again.json()["event_id"].as_str().unwrap(), first_event);

    let deliveries = get(&env, &format!("/api/v1/me/webhooks/{id}/deliveries"), &ta).await;
    assert_eq!(deliveries.status, StatusCode::OK, "{}", deliveries.body);
    let deliveries = deliveries.json();
    let list = deliveries.as_array().unwrap();
    assert_eq!(list.len(), 2, "{deliveries}");
    assert_ne!(list[0]["event_id"], list[1]["event_id"]);
    assert_eq!(
        list[1]["event_id"],
        first_event.as_str(),
        "newest first: {deliveries}"
    );
    let limited = get(
        &env,
        &format!("/api/v1/me/webhooks/{id}/deliveries?limit=1"),
        &ta,
    )
    .await;
    assert_eq!(limited.json().as_array().unwrap().len(), 1);

    // The stored body has the shape of every real delivery, so one parser serves both.
    let payload: String = env
        .store
        .read()
        .query_row(
            "SELECT payload FROM webhook_deliveries WHERE event_id = ?1",
            [&first_event],
            |r| r.get(0),
        )
        .unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(
        payload,
        json!({
            "id": first_event,
            "event": "webhook.test",
            "at": mtpool_protocol::rfc3339(env.clock.load(std::sync::atomic::Ordering::SeqCst) - 1000),
            "node": null,
            "domain": null,
            "detail": { "webhook_id": id },
        })
    );
    let hex = first_event.strip_prefix("evt_").expect("evt_ prefix");
    assert_eq!(hex.len(), 16);

    // Someone else's webhook looks like a missing one.
    for (method, path) in [
        ("POST", format!("/api/v1/me/webhooks/{id}/test")),
        ("GET", format!("/api/v1/me/webhooks/{id}/deliveries")),
        ("DELETE", format!("/api/v1/me/webhooks/{id}")),
    ] {
        let resp = call(&env, method, &path, &tb, None).await;
        assert_problem(&resp, StatusCode::NOT_FOUND, "not_found");
    }
    assert_eq!(count(&env, "SELECT count(*) FROM webhooks"), 1);

    let deleted = call(
        &env,
        "DELETE",
        &format!("/api/v1/me/webhooks/{id}"),
        &ta,
        None,
    )
    .await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
    let gone = get(&env, &format!("/api/v1/me/webhooks/{id}/deliveries"), &ta).await;
    assert_problem(&gone, StatusCode::NOT_FOUND, "not_found");
    assert_eq!(count(&env, "SELECT count(*) FROM webhook_deliveries"), 0);
}

#[tokio::test]
async fn bot_token_of_another_kind_is_refused() {
    let env = env(pool(vec![alpha()]));
    let a = owner(&env, "owner-a", 5);
    let enroll = token(&env, TokenKind::Enroll, Some(a));
    let resp = get(&env, "/api/v1/me", &enroll).await;
    assert!(
        resp.status == StatusCode::FORBIDDEN || resp.status == StatusCode::UNAUTHORIZED,
        "{}",
        resp.body
    );
}
