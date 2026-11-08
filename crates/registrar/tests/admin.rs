//! The admin API: settings, owners and tokens, node and domain actions, audit, events and
//! the live stream.

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use common::http::*;
use http_body_util::BodyExt;
use mtpool_core::{IpBlock, NodeState, PersistOp, Ts};
use mtpool_registrar::auth::tokens::TokenKind;
use serde_json::{Value, json};

const GP_SECRET: &str = "gp-secret-value-0123456789";
const CF_SECRET: &str = "cf-secret-value-9876543210";

fn alpha() -> mtpool_core::NodeRecord {
    node_at("alpha-00001", [45, 33, 1, 5], 1, NodeState::Healthy)
}

fn beta() -> mtpool_core::NodeRecord {
    node_at("beta-00002", [45, 33, 1, 6], 2, NodeState::Healthy)
}

async fn patch(env: &Env, token: &str, body: Value) -> Resp {
    call(env, "PATCH", "/api/v1/admin/settings", token, Some(body)).await
}

async fn audit(env: &Env, token: &str) -> Vec<Value> {
    env.store.flush().unwrap();
    let resp = get(env, "/api/v1/admin/audit?limit=200", token).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.body);
    resp.json().as_array().expect("an array").clone()
}

#[tokio::test]
async fn settings_secrets_write_only() {
    let env = env(pool(vec![alpha()]));
    let admin = admin_token(&env);

    let before = get(&env, "/api/v1/admin/settings", &admin).await;
    assert_eq!(before.status, StatusCode::OK, "{}", before.body);
    let before = before.json();
    assert_eq!(
        before["masters_per_domain"],
        live_settings(&env).await.masters_per_domain
    );
    for name in [
        "globalping_token",
        "redflag_api_key",
        "cloudflare_api_token",
    ] {
        assert!(before["secrets"][name].is_boolean(), "{name}: {before}");
    }

    let resp = patch(
        &env,
        &admin,
        json!({ "secrets": { "globalping_token": GP_SECRET, "cloudflare_api_token": CF_SECRET } }),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.body);
    assert_eq!(resp.json()["secrets"]["globalping_token"], true);
    assert_eq!(resp.json()["secrets"]["cloudflare_api_token"], true);
    assert_eq!(count(&env, "SELECT count(*) FROM secrets"), 2);

    // The value reaches the lookup the workers use.
    assert_eq!(
        mtpool_registrar::secrets::secret(&env.store, "globalping_token").as_deref(),
        Some(GP_SECRET)
    );

    // And nothing shows it: not the document, not the audit.
    let after = get(&env, "/api/v1/admin/settings", &admin).await;
    for secret in [GP_SECRET, CF_SECRET] {
        assert!(!resp.body.contains(secret), "{}", resp.body);
        assert!(!after.body.contains(secret), "{}", after.body);
    }
    let entries = audit(&env, &admin).await;
    let text = serde_json::to_string(&entries).unwrap();
    for secret in [GP_SECRET, CF_SECRET] {
        assert!(!text.contains(secret), "{text}");
    }
    assert!(
        text.contains("globalping_token"),
        "the name is logged: {text}"
    );

    // null clears the stored value.
    let cleared = patch(
        &env,
        &admin,
        json!({ "secrets": { "globalping_token": null } }),
    )
    .await;
    assert_eq!(cleared.status, StatusCode::OK, "{}", cleared.body);
    assert_eq!(count(&env, "SELECT count(*) FROM secrets"), 1);

    // An unknown secret name is refused.
    let unknown = patch(&env, &admin, json!({ "secrets": { "nope": "x" } })).await;
    assert_problem(&unknown, StatusCode::BAD_REQUEST, "bad_request");
}

#[tokio::test]
async fn settings_patch_applies_without_restart() {
    let env = env(pool(vec![alpha()]));
    let admin = admin_token(&env);

    let resp = patch(
        &env,
        &admin,
        json!({ "masters_per_domain": 3, "srmd": { "max_nodes_per_domain": 7 } }),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.body);
    let body = resp.json();
    assert_eq!(body["masters_per_domain"], 3, "{body}");
    assert_eq!(body["srmd"]["max_nodes_per_domain"], 7, "{body}");

    let live = live_settings(&env).await;
    assert_eq!(live.masters_per_domain, 3);
    assert_eq!(live.srmd.max_nodes_per_domain, 7);
    // The rest of the nested object and the other fields are untouched.
    let default = mtpool_core::Settings::default();
    assert_eq!(live.srmd.max_domains, default.srmd.max_domains);
    assert_eq!(live.tls_domain, "tls.example.com");

    let read = get(&env, "/api/v1/admin/settings", &admin).await.json();
    assert_eq!(read["masters_per_domain"], 3);
    assert_eq!(read["srmd"]["max_nodes_per_domain"], 7);

    // Persisted for the next start.
    assert!(count(&env, "SELECT count(*) FROM settings") >= 1);
}

#[tokio::test]
async fn globalping_settings_reach_the_sensor_without_restart() {
    let env = env(pool(vec![alpha()]));
    let admin = admin_token(&env);
    assert_eq!(env.gp_settings.load().success_threshold, 0.5);

    let resp = patch(
        &env,
        &admin,
        json!({ "globalping": { "success_threshold": 0.75, "allow_credits": true } }),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.body);

    let live = env.gp_settings.load();
    assert_eq!(live.success_threshold, 0.75);
    assert!(live.allow_credits);
    // The rest of the Globalping settings stays as it was.
    assert_eq!(
        live.probe_limit,
        mtpool_core::GlobalpingSettings::default().probe_limit
    );
}

#[tokio::test]
async fn invalid_settings_rejected_nothing_changed() {
    let env = env(pool(vec![alpha()]));
    let admin = admin_token(&env);
    let before = live_settings(&env).await;

    let zero = patch(
        &env,
        &admin,
        json!({
            "masters_per_domain": 0,
            "secrets": { "globalping_token": GP_SECRET }
        }),
    )
    .await;
    assert_problem(&zero, StatusCode::BAD_REQUEST, "invalid_settings");
    assert!(zero.body.contains("masters_per_domain"), "{}", zero.body);
    assert!(!zero.body.contains(GP_SECRET));

    let unknown = patch(&env, &admin, json!({ "no_such_field": 1 })).await;
    assert_problem(&unknown, StatusCode::BAD_REQUEST, "bad_request");
    assert!(unknown.body.contains("no_such_field"), "{}", unknown.body);

    let nested = patch(&env, &admin, json!({ "srmd": { "no_such_field": 1 } })).await;
    assert_problem(&nested, StatusCode::BAD_REQUEST, "bad_request");

    let wrong_type = patch(&env, &admin, json!({ "masters_per_domain": "many" })).await;
    assert_eq!(
        wrong_type.status,
        StatusCode::BAD_REQUEST,
        "{}",
        wrong_type.body
    );

    let not_object = patch(&env, &admin, json!([1, 2])).await;
    assert_eq!(
        not_object.status,
        StatusCode::BAD_REQUEST,
        "{}",
        not_object.body
    );

    assert_eq!(live_settings(&env).await, before, "nothing applied");
    assert_eq!(
        count(&env, "SELECT count(*) FROM secrets"),
        0,
        "no secret written"
    );
    assert!(
        audit(&env, &admin).await.is_empty(),
        "a refused patch changes nothing and logs nothing"
    );
}

#[tokio::test]
async fn admin_actions_written_to_audit() {
    let env = env(pool(vec![alpha()]));
    let admin = admin_token(&env);

    let owner_resp = post(
        &env,
        "/api/v1/admin/owners",
        &admin,
        Some(json!({ "name": "audit-owner", "node_limit": 3, "note": "n" })),
    )
    .await;
    assert_eq!(
        owner_resp.status,
        StatusCode::CREATED,
        "{}",
        owner_resp.body
    );
    let owner_id = owner_resp.json()["id"].as_i64().unwrap();

    let settings_resp = patch(&env, &admin, json!({ "masters_per_domain": 2 })).await;
    assert_eq!(
        settings_resp.status,
        StatusCode::OK,
        "{}",
        settings_resp.body
    );

    let token_resp = post(
        &env,
        "/api/v1/admin/tokens",
        &admin,
        Some(json!({ "kind": "bot", "owner_id": owner_id })),
    )
    .await;
    assert_eq!(
        token_resp.status,
        StatusCode::CREATED,
        "{}",
        token_resp.body
    );
    let created = token_resp.json()["token"].as_str().unwrap().to_owned();

    let retire = post(&env, "/api/v1/admin/nodes/alpha-00001/retire", &admin, None).await;
    assert_eq!(retire.status, StatusCode::OK, "{}", retire.body);

    let entries = audit(&env, &admin).await;
    let actions: Vec<&str> = entries
        .iter()
        .map(|e| e["action"].as_str().unwrap())
        .collect();
    assert_eq!(
        actions,
        [
            "node.retire",
            "token.create",
            "settings.update",
            "owner.create"
        ],
        "newest first: {entries:?}"
    );
    for entry in &entries {
        assert!(
            entry["actor"].as_str().unwrap().starts_with("token:"),
            "{entry}"
        );
        assert!(entry["at"].is_string(), "{entry}");
        assert!(entry["id"].is_number(), "{entry}");
    }
    assert_eq!(entries[0]["target"], "alpha-00001");
    assert_eq!(
        entries[2]["detail"]["fields"],
        json!(["masters_per_domain"])
    );
    assert!(!serde_json::to_string(&entries).unwrap().contains(&created));

    // Paging and the actor of an unauthenticated call.
    let page = get(&env, "/api/v1/admin/audit?limit=2", &admin)
        .await
        .json();
    assert_eq!(page.as_array().unwrap().len(), 2);
    let last_id = page[1]["id"].as_i64().unwrap();
    let rest = get(
        &env,
        &format!("/api/v1/admin/audit?limit=2&before={last_id}"),
        &admin,
    )
    .await
    .json();
    assert_eq!(rest.as_array().unwrap().len(), 2);
    let anonymous = send(&env, request("GET", "/api/v1/admin/audit", None, None)).await;
    assert_problem(&anonymous, StatusCode::UNAUTHORIZED, "unauthorized");
}

#[tokio::test]
async fn create_token_shows_value_once() {
    let env = env(pool(vec![alpha()]));
    let admin = admin_token(&env);
    let a = owner(&env, "owner-a", 5);

    let created = post(
        &env,
        "/api/v1/admin/tokens",
        &admin,
        Some(json!({ "kind": "bot", "owner_id": a.0 })),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let created = created.json();
    let value = created["token"].as_str().unwrap().to_owned();
    assert!(value.starts_with("mtp_bot_"), "{created}");
    let id = created["id"].as_i64().unwrap();
    assert_eq!(created["kind"], "bot");
    assert_eq!(created["owner_id"], a.0);

    // It works for the owner's endpoints.
    assert_eq!(get(&env, "/api/v1/me", &value).await.status, StatusCode::OK);

    // The listing shows metadata only.
    let list = get(&env, &format!("/api/v1/admin/tokens?owner={}", a.0), &admin).await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.body);
    assert!(!list.body.contains(&value), "{}", list.body);
    assert!(!list.body.contains("hash"), "{}", list.body);
    let row = list
        .json()
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == id)
        .cloned()
        .expect("the new token is listed");
    assert_eq!(row["kind"], "bot");
    assert!(row["prefix"].as_str().unwrap().starts_with("mtp_bot_"));
    assert!(row["revoked_at"].is_null());
    assert!(row.get("token").is_none());

    // Only enroll, bot and metrics tokens can be made here.
    for kind in ["admin", "node"] {
        let resp = post(
            &env,
            "/api/v1/admin/tokens",
            &admin,
            Some(json!({ "kind": kind, "owner_id": a.0 })),
        )
        .await;
        assert_problem(&resp, StatusCode::BAD_REQUEST, "bad_request");
    }
    let nobody = post(
        &env,
        "/api/v1/admin/tokens",
        &admin,
        Some(json!({ "kind": "bot", "owner_id": 999 })),
    )
    .await;
    assert_problem(&nobody, StatusCode::BAD_REQUEST, "unknown_owner");
    let metrics = post(
        &env,
        "/api/v1/admin/tokens",
        &admin,
        Some(json!({ "kind": "metrics" })),
    )
    .await;
    assert_eq!(metrics.status, StatusCode::CREATED, "{}", metrics.body);

    // A bot token cannot reach the admin API.
    let denied = get(&env, "/api/v1/admin/tokens", &value).await;
    assert!(
        denied.status == StatusCode::FORBIDDEN || denied.status == StatusCode::UNAUTHORIZED,
        "{}",
        denied.body
    );

    // Revoking stops it.
    let revoked = call(
        &env,
        "DELETE",
        &format!("/api/v1/admin/tokens/{id}"),
        &admin,
        None,
    )
    .await;
    assert_eq!(revoked.status, StatusCode::OK, "{}", revoked.body);
    assert_problem(
        &get(&env, "/api/v1/me", &value).await,
        StatusCode::UNAUTHORIZED,
        "unauthorized",
    );
    let missing = call(&env, "DELETE", "/api/v1/admin/tokens/99999", &admin, None).await;
    assert_problem(&missing, StatusCode::NOT_FOUND, "not_found");
}

#[tokio::test]
async fn owner_crud_and_tokens_bound_to_owner() {
    let env = env(pool(vec![alpha(), beta()]));
    let admin = admin_token(&env);

    let a = post(
        &env,
        "/api/v1/admin/owners",
        &admin,
        Some(json!({ "name": "owner-a", "node_limit": 4 })),
    )
    .await;
    assert_eq!(a.status, StatusCode::CREATED, "{}", a.body);
    let a_id = a.json()["id"].as_i64().unwrap();
    let b = post(
        &env,
        "/api/v1/admin/owners",
        &admin,
        Some(json!({ "name": "owner-b", "node_limit": 4 })),
    )
    .await;
    let b_id = b.json()["id"].as_i64().unwrap();
    assert_eq!((a_id, b_id), (1, 2));

    let duplicate = post(
        &env,
        "/api/v1/admin/owners",
        &admin,
        Some(json!({ "name": "owner-a", "node_limit": 1 })),
    )
    .await;
    assert_problem(&duplicate, StatusCode::CONFLICT, "owner_exists");

    let updated = call(
        &env,
        "PATCH",
        &format!("/api/v1/admin/owners/{a_id}"),
        &admin,
        Some(json!({ "node_limit": 9, "note": "renamed" })),
    )
    .await;
    assert_eq!(updated.status, StatusCode::OK, "{}", updated.body);
    assert_eq!(updated.json()["node_limit"], 9);
    assert_eq!(updated.json()["note"], "renamed");
    assert_eq!(updated.json()["name"], "owner-a");
    let missing = call(
        &env,
        "PATCH",
        "/api/v1/admin/owners/99",
        &admin,
        Some(json!({ "note": "x" })),
    )
    .await;
    assert_problem(&missing, StatusCode::NOT_FOUND, "not_found");

    let list = get(&env, "/api/v1/admin/owners", &admin).await.json();
    assert_eq!(list.as_array().unwrap().len(), 2, "{list}");

    // A bot token of an owner sees only that owner's nodes.
    let token_a = bot_token(&env, mtpool_core::OwnerId(a_id));
    let token_b = bot_token(&env, mtpool_core::OwnerId(b_id));
    let nodes_a = get(&env, "/api/v1/me/nodes", &token_a).await.json();
    let nodes_b = get(&env, "/api/v1/me/nodes", &token_b).await.json();
    assert_eq!(nodes_a.as_array().unwrap().len(), 1);
    assert_eq!(nodes_a[0]["id"], "alpha-00001");
    assert_eq!(nodes_b.as_array().unwrap().len(), 1);
    assert_eq!(nodes_b[0]["id"], "beta-00002");

    // An owner with live nodes cannot be deleted.
    let blocked = call(
        &env,
        "DELETE",
        &format!("/api/v1/admin/owners/{a_id}"),
        &admin,
        None,
    )
    .await;
    assert_problem(&blocked, StatusCode::CONFLICT, "owner_has_nodes");
    assert_eq!(
        get(&env, "/api/v1/me", &token_a).await.status,
        StatusCode::OK
    );

    // Once the node is gone the owner can go; its tokens stop working.
    let retire = post(&env, "/api/v1/admin/nodes/alpha-00001/retire", &admin, None).await;
    assert_eq!(retire.status, StatusCode::OK, "{}", retire.body);
    let deleted = call(
        &env,
        "DELETE",
        &format!("/api/v1/admin/owners/{a_id}"),
        &admin,
        None,
    )
    .await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
    assert_problem(
        &get(&env, "/api/v1/me", &token_a).await,
        StatusCode::UNAUTHORIZED,
        "unauthorized",
    );
    let list = get(&env, "/api/v1/admin/owners", &admin).await.json();
    assert_eq!(list.as_array().unwrap().len(), 1, "{list}");
    assert_eq!(list[0]["id"], b_id);
    // The name is free again.
    let again = post(
        &env,
        "/api/v1/admin/owners",
        &admin,
        Some(json!({ "name": "owner-a", "node_limit": 1 })),
    )
    .await;
    assert_eq!(again.status, StatusCode::CREATED, "{}", again.body);
    assert_ne!(
        again.json()["id"].as_i64().unwrap(),
        a_id,
        "ids are not reused"
    );
    let gone = call(
        &env,
        "DELETE",
        &format!("/api/v1/admin/owners/{a_id}"),
        &admin,
        None,
    )
    .await;
    assert_problem(&gone, StatusCode::NOT_FOUND, "not_found");
}

#[tokio::test]
async fn stream_sends_events_and_keepalive_with_no_buffer_header() {
    let env = env_with_keepalive(pool(vec![alpha()]), Duration::from_millis(50));
    let admin = admin_token(&env);

    let response = {
        use tower::ServiceExt;
        env.app
            .clone()
            .oneshot(request("GET", "/api/v1/admin/stream", Some(&admin), None))
            .await
            .unwrap()
    };
    assert_eq!(response.status(), StatusCode::OK);
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_owned())
    };
    assert!(
        header("content-type")
            .unwrap()
            .starts_with("text/event-stream")
    );
    assert_eq!(header("x-accel-buffering").as_deref(), Some("no"));
    assert_eq!(
        header("cache-control").as_deref(),
        Some("no-cache, no-transform")
    );

    let mut body = response.into_body();
    let mut seen = String::new();
    let read_until = |needle: &'static str| async move { needle };
    let _ = read_until;

    async fn pump(body: &mut axum::body::Body, seen: &mut String, needle: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !seen.contains(needle) {
            let frame = tokio::time::timeout_at(deadline, body.frame())
                .await
                .unwrap_or_else(|_| panic!("no {needle:?} in {seen:?}"))
                .unwrap_or_else(|| panic!("the stream ended before {needle:?}: {seen:?}"))
                .expect("frame");
            if let Ok(data) = frame.into_data() {
                seen.push_str(std::str::from_utf8(&data).unwrap());
            }
        }
    }

    pump(&mut body, &mut seen, ": keepalive").await;

    let retire = post(&env, "/api/v1/admin/nodes/alpha-00001/retire", &admin, None).await;
    assert_eq!(retire.status, StatusCode::OK, "{}", retire.body);
    pump(&mut body, &mut seen, "event: node_dead").await;
    assert!(seen.contains("id: "), "{seen}");
    let data_line = seen
        .lines()
        .find(|l| l.starts_with("data: ") && l.contains("node_dead"))
        .expect("a data line");
    let data: Value = serde_json::from_str(&data_line["data: ".len()..]).unwrap();
    assert_eq!(data["kind"], "node_dead");
    assert_eq!(data["node"], "alpha-00001");

    // The stream ends when the actor stops.
    env.actor.stop().await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match tokio::time::timeout_at(deadline, body.frame()).await {
            Err(_) => panic!("the stream outlived the actor"),
            Ok(None) => break,
            Ok(Some(_)) => {}
        }
    }
}

#[tokio::test]
async fn stream_requires_an_admin_token() {
    let env = env(pool(vec![alpha()]));
    let anonymous = send(&env, request("GET", "/api/v1/admin/stream", None, None)).await;
    assert_problem(&anonymous, StatusCode::UNAUTHORIZED, "unauthorized");
}

#[tokio::test]
async fn token_list_names_the_node_and_filters_by_kind() {
    let env = env(pool(vec![alpha()]));
    let admin = admin_token(&env);
    let a = owner(&env, "a", 5);
    token(&env, TokenKind::Enroll, Some(a));
    mtpool_registrar::auth::tokens::issue(
        &env.store,
        mtpool_registrar::auth::tokens::NewToken {
            kind: TokenKind::Node,
            owner: None,
            node: Some(alpha().id),
            expires_at: None,
            scope: String::new(),
        },
    )
    .unwrap();

    let all = get(&env, "/api/v1/admin/tokens", &admin).await.json();
    assert_eq!(all.as_array().unwrap().len(), 3, "{all}");
    assert_eq!(all[0]["kind"], "node");
    assert_eq!(all[0]["node_id"], "alpha-00001");
    assert!(all[1].get("node_id").is_none(), "{all}");

    let nodes = get(&env, "/api/v1/admin/tokens?kind=node", &admin)
        .await
        .json();
    assert_eq!(nodes.as_array().unwrap().len(), 1, "{nodes}");
    assert_eq!(nodes[0]["node_id"], "alpha-00001");

    let scoped = get(
        &env,
        &format!("/api/v1/admin/tokens?kind=enroll&owner={}", a.0),
        &admin,
    )
    .await
    .json();
    assert_eq!(scoped.as_array().unwrap().len(), 1, "{scoped}");

    let none = get(&env, "/api/v1/admin/tokens?kind=bot", &admin)
        .await
        .json();
    assert_eq!(none, json!([]));

    let bad = get(&env, "/api/v1/admin/tokens?kind=nope", &admin).await;
    assert_problem(&bad, StatusCode::BAD_REQUEST, "bad_request");
}

#[tokio::test]
async fn a_quarantined_node_says_why_and_when_it_was_last_measured() {
    let mut sick = alpha();
    sick.state = NodeState::Quarantine;
    sick.quarantine_reasons = vec![
        mtpool_core::FailedCondition::ReportStale,
        mtpool_core::FailedCondition::Tcp,
    ];
    sick.health.last_report = Ts(NOW - 90_000);
    let mut fresh = beta();
    fresh.health.last_report = Ts(0);
    let env = env(pool(vec![sick, fresh]));
    let admin = admin_token(&env);

    let body = get(&env, "/api/v1/admin/nodes/alpha-00001", &admin)
        .await
        .json();
    assert_eq!(
        body["quarantine_reasons"],
        json!(["report_stale", "tcp"]),
        "{body}"
    );
    assert_eq!(
        body["last_report_at"],
        mtpool_protocol::rfc3339(NOW - 90_000)
    );
    let public = get(&env, "/api/v1/nodes/alpha-00001", "").await.json();
    assert_eq!(
        public["quarantine_reasons"],
        json!(["report_stale", "tcp"]),
        "{public}"
    );
    assert!(public.get("last_report_at").is_none(), "{public}");

    let body = get(&env, "/api/v1/admin/nodes/beta-00002", &admin)
        .await
        .json();
    assert!(body.get("quarantine_reasons").is_none(), "{body}");
    assert!(body.get("last_report_at").is_none(), "{body}");
}

/// The ban log names every node that failed on an address; the block only the first of them.
/// Whichever row the admin starts from, the ban of that address is what gets lifted.
#[tokio::test]
async fn unban_works_from_the_id_of_any_open_row_of_the_ban_log() {
    let mut state = pool(vec![alpha()]);
    state.blocks.push(IpBlock {
        ip: beta().ip,
        node_id: beta().id,
        blocked_at: Ts(NOW),
        next_reverify_at: Some(Ts(NOW + 900_000)),
        owner: Some(mtpool_core::OwnerId(2)),
    });
    let env = env(state);
    let admin = admin_token(&env);
    env.store.send(PersistOp::RecordBan {
        ip: beta().ip,
        node: mtpool_core::NodeId::parse("again-00003").unwrap(),
        at: Ts(NOW),
        votes: vec![],
        reverify: true,
        node_registered_at: None,
    });
    env.store.flush().unwrap();

    let unban = post(&env, "/api/v1/admin/nodes/again-00003/unban", &admin, None).await;
    assert_eq!(unban.status, StatusCode::OK, "{}", unban.body);
    let blocks = env.actor.inspect(|s| s.blocks.clone()).await.unwrap();
    assert!(blocks.is_empty());

    env.store.flush().unwrap();
    let rows = get(&env, "/api/v1/bans", &admin).await.json();
    assert!(!rows[0]["lifted_at"].is_null(), "{rows}");
    let events = get(&env, "/api/v1/admin/events?kind=ban_lifted", &admin)
        .await
        .json();
    assert_eq!(events[0]["node"], "beta-00002", "{events}");
    assert_eq!(events[0]["detail"], json!({ "admin": true }), "{events}");

    // Lifted: the row names nothing to lift any more.
    let again = post(&env, "/api/v1/admin/nodes/again-00003/unban", &admin, None).await;
    assert_problem(&again, StatusCode::NOT_FOUND, "not_found");
}

#[tokio::test]
async fn node_actions_domains_and_events() {
    // A ban leaves a block and no record.
    let mut state = pool(vec![alpha()]);
    state.blocks.push(IpBlock {
        ip: beta().ip,
        node_id: beta().id,
        blocked_at: Ts(NOW),
        next_reverify_at: None,
        owner: None,
    });
    let env = env(state);
    let admin = admin_token(&env);

    // Unknown node, a node that is not banned, a node that is banned.
    let unknown = post(
        &env,
        "/api/v1/admin/nodes/nobody-00009/retire",
        &admin,
        None,
    )
    .await;
    assert_problem(&unknown, StatusCode::NOT_FOUND, "not_found");
    let not_banned = post(&env, "/api/v1/admin/nodes/alpha-00001/unban", &admin, None).await;
    assert_problem(&not_banned, StatusCode::CONFLICT, "not_banned");
    let healthy = post(
        &env,
        "/api/v1/admin/nodes/alpha-00001/recheck",
        &admin,
        None,
    )
    .await;
    assert_problem(&healthy, StatusCode::CONFLICT, "not_recheckable");
    let unban = post(&env, "/api/v1/admin/nodes/beta-00002/unban", &admin, None).await;
    assert_eq!(unban.status, StatusCode::OK, "{}", unban.body);
    let again = post(&env, "/api/v1/admin/nodes/beta-00002/unban", &admin, None).await;
    assert_problem(&again, StatusCode::NOT_FOUND, "not_found");

    // The events table has the lift.
    env.store.flush().unwrap();
    let events = get(&env, "/api/v1/admin/events?kind=ban_lifted", &admin).await;
    assert_eq!(events.status, StatusCode::OK, "{}", events.body);
    let events = events.json();
    assert_eq!(events.as_array().unwrap().len(), 1, "{events}");
    assert_eq!(events[0]["node"], "beta-00002");
    assert!(events[0]["at"].is_string());

    // Domains edit the live settings.
    let domains = get(&env, "/api/v1/admin/domains", &admin).await.json();
    assert_eq!(domains, json!(["proxy.example.com"]));
    let added = post(
        &env,
        "/api/v1/admin/domains",
        &admin,
        Some(json!({ "domain": "two.example.com" })),
    )
    .await;
    assert_eq!(added.status, StatusCode::CREATED, "{}", added.body);
    assert_eq!(live_settings(&env).await.domains.len(), 2);
    let dup = post(
        &env,
        "/api/v1/admin/domains",
        &admin,
        Some(json!({ "domain": "two.example.com" })),
    )
    .await;
    assert_problem(&dup, StatusCode::CONFLICT, "domain_exists");
    let removed = call(
        &env,
        "DELETE",
        "/api/v1/admin/domains/two.example.com",
        &admin,
        None,
    )
    .await;
    assert_eq!(removed.status, StatusCode::OK, "{}", removed.body);
    assert_eq!(
        live_settings(&env).await.domains,
        vec!["proxy.example.com".to_owned()]
    );
    let absent = call(
        &env,
        "DELETE",
        "/api/v1/admin/domains/two.example.com",
        &admin,
        None,
    )
    .await;
    assert_problem(&absent, StatusCode::NOT_FOUND, "not_found");

    // SRMD manual control of a domain that is not configured.
    let take = post(
        &env,
        "/api/v1/admin/srmd/unknown.example.com/take",
        &admin,
        None,
    )
    .await;
    assert_problem(&take, StatusCode::NOT_FOUND, "not_found");
}

#[tokio::test]
async fn admin_api_has_no_public_rate_limit_and_needs_a_token() {
    let env = env(pool(vec![alpha()]));
    let anonymous = send(&env, request("GET", "/api/v1/admin/owners", None, None)).await;
    assert_problem(&anonymous, StatusCode::UNAUTHORIZED, "unauthorized");
    let admin = admin_token(&env);
    let owner_token = bot_token(&env, owner(&env, "owner-a", 1));
    let denied = get(&env, "/api/v1/admin/owners", &owner_token).await;
    assert!(
        denied.status == StatusCode::FORBIDDEN || denied.status == StatusCode::UNAUTHORIZED,
        "{}",
        denied.body
    );
    let _ = token(&env, TokenKind::Metrics, None);
    assert_eq!(
        get(&env, "/api/v1/admin/owners", &admin).await.status,
        StatusCode::OK
    );
}

// ----- the panel's read paths ----------------------------------------------------------------

fn id_of(name: &str) -> mtpool_core::NodeId {
    mtpool_core::NodeId::parse(name).expect("a node id")
}

fn round(
    env: &Env,
    node: &str,
    at: i64,
    verdict: mtpool_core::RoundVerdict,
    votes: Vec<(mtpool_core::SensorKind, mtpool_core::SensorVote)>,
) {
    env.store.send(mtpool_core::PersistOp::RecordRound {
        node: id_of(node),
        generation: mtpool_core::Generation(1),
        at: mtpool_core::Ts(at),
        verdict,
        votes,
    });
}

fn log(env: &Env, kind: mtpool_core::EventKind, at: i64, detail: Value) {
    env.store.send(mtpool_core::Event {
        kind,
        at: mtpool_core::Ts(at),
        node: None,
        owner: None,
        domain: None,
        detail,
    });
}

#[tokio::test]
async fn the_new_admin_paths_need_a_login() {
    let env = env(pool(vec![alpha()]));
    for path in [
        "/api/v1/admin/nodes",
        "/api/v1/admin/nodes/alpha-00001",
        "/api/v1/admin/status",
        "/api/v1/admin/srmd",
    ] {
        let resp = send(&env, request("GET", path, None, None)).await;
        assert_problem(&resp, StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let resp = send(&env, request("POST", "/api/v1/admin/dns/push", None, None)).await;
    assert_problem(&resp, StatusCode::UNAUTHORIZED, "unauthorized");
}

#[tokio::test]
async fn admin_nodes_list_every_node_with_the_whole_address_and_the_owner() {
    let mut banned = beta();
    banned.state = NodeState::Banned;
    let env = env(pool(vec![alpha(), banned]));
    let admin = admin_token(&env);
    assert_eq!(owner(&env, "owner-a", 1), mtpool_core::OwnerId(1));
    assert_eq!(owner(&env, "owner-b", 1), mtpool_core::OwnerId(2));

    // The version comes from the rollout record, and only once the node has polled.
    assert!(env.rollouts.offer_release(
        "2.0.0",
        mtpool_core::Ts(NOW),
        mtpool_core::RolloutSettings::default()
    ));
    env.rollouts
        .poll(&id_of("alpha-00001"), "1.9.0", mtpool_core::Ts(NOW));

    let resp = get(&env, "/api/v1/admin/nodes", &admin).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.body);
    let nodes = resp.json();
    let nodes = nodes.as_array().expect("an array");
    assert_eq!(nodes.len(), 2, "the banned node is listed too");
    let a = nodes.iter().find(|n| n["id"] == "alpha-00001").unwrap();
    let b = nodes.iter().find(|n| n["id"] == "beta-00002").unwrap();
    assert_eq!(a["full_ip"], "45.33.1.5");
    assert_eq!(a["ip"], "45.x.x.5", "the public part stays masked");
    assert_eq!(a["owner_id"], 1);
    assert_eq!(a["owner_name"], "owner-a");
    assert_eq!(a["agent_version"], "1.9.0");
    assert!(a["last_heartbeat"].is_string(), "{a}");
    assert!(a["failed"].is_array(), "{a}");
    assert_eq!(b["state"], "banned");
    assert_eq!(b["full_ip"], "45.33.1.6");
    assert_eq!(b["owner_name"], "owner-b");
    assert!(b.get("agent_version").is_none(), "never polled: {b}");

    // The public list still leaves the banned node out and never shows the whole address.
    let public = send(&env, request("GET", "/api/v1/nodes", None, None)).await;
    assert!(!public.body.contains("45.33.1"), "{}", public.body);
    assert!(!public.body.contains("beta-00002"), "{}", public.body);
}

#[tokio::test]
async fn an_admin_node_has_its_rounds_with_evidence_newest_first() {
    use mtpool_core::{AbstainReason, RoundVerdict, SensorKind, SensorVote, Vote};
    let env = env(pool(vec![alpha()]));
    let admin = admin_token(&env);
    round(
        &env,
        "alpha-00001",
        NOW - 120_000,
        RoundVerdict::Clean,
        vec![(
            SensorKind::Cheburcheck,
            SensorVote {
                vote: Vote::Clear,
                evidence: json!({"checked": "old"}),
            },
        )],
    );
    round(
        &env,
        "alpha-00001",
        NOW - 60_000,
        RoundVerdict::Undecided,
        vec![
            (
                SensorKind::Globalping,
                SensorVote {
                    vote: Vote::Blocked,
                    evidence: json!({"probes": 12}),
                },
            ),
            (
                SensorKind::RedFlag,
                SensorVote {
                    vote: Vote::Abstain(AbstainReason::Timeout),
                    evidence: json!({"waited_ms": 5000}),
                },
            ),
        ],
    );
    env.store.flush().unwrap();

    let resp = get(&env, "/api/v1/admin/nodes/alpha-00001", &admin).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.body);
    let node = resp.json();
    assert_eq!(node["id"], "alpha-00001");
    assert_eq!(node["full_ip"], "45.33.1.5");
    let rounds = node["rounds"].as_array().expect("rounds");
    assert_eq!(rounds.len(), 2);
    assert_eq!(rounds[0]["verdict"], "undecided");
    assert_eq!(rounds[1]["verdict"], "clean");
    let votes = rounds[0]["votes"].as_array().unwrap();
    let gp = votes.iter().find(|v| v["sensor"] == "globalping").unwrap();
    assert_eq!(gp["vote"], "blocked");
    assert_eq!(gp["evidence"], json!({"probes": 12}));
    let rf = votes.iter().find(|v| v["sensor"] == "red_flag").unwrap();
    assert_eq!(rf["vote"], "abstain");
    assert_eq!(rf["reason"], "timeout");
    assert_eq!(rf["evidence"], json!({"waited_ms": 5000}));

    // The evidence stays in the admin API.
    let public = send(
        &env,
        request("GET", "/api/v1/nodes/alpha-00001", None, None),
    )
    .await;
    assert!(!public.body.contains("probes"), "{}", public.body);
    assert!(!public.body.contains("evidence"), "{}", public.body);

    let unknown = get(&env, "/api/v1/admin/nodes/nobody-00009", &admin).await;
    assert_problem(&unknown, StatusCode::NOT_FOUND, "not_found");
}

#[tokio::test]
async fn status_shows_sensors_dns_and_counters() {
    use mtpool_core::{AbstainReason, EventKind, RoundVerdict, SensorKind, SensorVote, Vote};
    let env = env(pool(vec![alpha()]));
    let admin = admin_token(&env);
    round(
        &env,
        "alpha-00001",
        NOW - 60_000,
        RoundVerdict::Undecided,
        vec![
            (
                SensorKind::Globalping,
                SensorVote {
                    vote: Vote::Blocked,
                    evidence: json!({}),
                },
            ),
            (
                SensorKind::RedFlag,
                SensorVote {
                    vote: Vote::Abstain(AbstainReason::Timeout),
                    evidence: json!({}),
                },
            ),
        ],
    );
    let day = 86_400_000;
    log(&env, EventKind::DnsApplied, NOW - 1_000, json!({}));
    log(&env, EventKind::DnsApplied, NOW - 2_000, json!({}));
    log(&env, EventKind::DnsApplied, NOW - 3 * day, json!({}));
    log(
        &env,
        EventKind::DnsFailed,
        NOW - 500,
        json!({"error": "provider said no"}),
    );
    log(&env, EventKind::MasterChanged, NOW - 100, json!({}));
    log(&env, EventKind::MasterChanged, NOW - 2 * day, json!({}));
    let ban = |at: i64, reverify: bool| {
        env.store.send(mtpool_core::PersistOp::RecordBan {
            ip: std::net::Ipv4Addr::new(203, 0, 113, 7),
            node: id_of("alpha-00001"),
            at: mtpool_core::Ts(at),
            votes: vec![(SensorKind::Globalping, Vote::Blocked)],
            reverify,
            node_registered_at: None,
        });
    };
    ban(NOW - 1_000, false);
    ban(NOW - 2_000, true);
    ban(NOW - 8 * day, false);
    env.store.flush().unwrap();
    let set = patch(
        &env,
        &admin,
        json!({ "secrets": { "globalping_token": GP_SECRET, "cloudflare_api_token": CF_SECRET } }),
    )
    .await;
    assert_eq!(set.status, StatusCode::OK, "{}", set.body);

    let resp = get(&env, "/api/v1/admin/status", &admin).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.body);
    assert!(!resp.body.contains(GP_SECRET) && !resp.body.contains(CF_SECRET));
    let status = resp.json();
    let sensor = |name: &str| {
        status["sensors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == name)
            .unwrap_or_else(|| panic!("no sensor {name}: {status}"))
            .clone()
    };
    let gp = sensor("globalping");
    assert_eq!(gp["configured"], true);
    assert_eq!(gp["last_vote"], "blocked");
    assert_eq!(gp["last_answer_at"], mtpool_protocol::rfc3339(NOW - 60_000));
    assert_eq!(sensor("check_host")["configured"], true);
    assert_eq!(
        sensor("cheburcheck")["configured"],
        false,
        "no sensor of it remains"
    );
    let rf = sensor("red_flag");
    assert_eq!(rf["last_vote"], "abstain");
    assert_eq!(rf["last_reason"], "timeout");
    assert!(
        rf.get("last_answer_at").is_none(),
        "only an abstention: {rf}"
    );

    let dns = &status["dns"];
    assert_eq!(dns["token_set"], true);
    assert_eq!(dns["applied_24h"], 2, "{dns}");
    assert_eq!(dns["failed_24h"], 1, "{dns}");
    assert_eq!(
        dns["last_applied_at"],
        mtpool_protocol::rfc3339(NOW - 1_000)
    );
    assert_eq!(dns["last_failed_at"], mtpool_protocol::rfc3339(NOW - 500));
    assert_eq!(dns["last_error"], "provider said no");
    assert_eq!(dns["unsynced_domains"], 1, "no DNS entry yet: {dns}");
    assert_eq!(status["counts"]["master_changes_24h"], 1);
    assert_eq!(
        status["counts"]["bans_7d"], 1,
        "a failed reverify and an old ban do not count"
    );
}

#[tokio::test]
async fn srmd_overview_reports_the_domains_and_how_many_are_needed() {
    let mut state = pool(vec![alpha(), beta()]);
    state.settings.domains = vec!["proxy.example.com".into(), "edge.example.com".into()];
    state
        .srmd
        .cnames
        .insert("edge.example.com".into(), "proxy.example.com".into());
    state.srmd.created.insert("edge.example.com".into());
    state
        .srmd
        .domain_clients
        .insert("proxy.example.com".into(), 42);
    let env = env(state);
    let admin = admin_token(&env);

    let resp = get(&env, "/api/v1/admin/srmd", &admin).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.body);
    let body = resp.json();
    assert_eq!(body["active"], json!(["proxy.example.com"]));
    assert_eq!(body["folded"], json!(["edge.example.com"]));
    assert_eq!(body["created"], json!(["edge.example.com"]));
    assert_eq!(
        body["cnames"],
        json!({"edge.example.com": "proxy.example.com"})
    );
    assert_eq!(body["clients"], json!({"proxy.example.com": 42}));
    assert_eq!(body["needed"], 1);
    assert!(body["enabled"].is_boolean(), "{body}");
}

#[tokio::test]
async fn dns_push_marks_every_domain_but_one_that_is_being_pushed() {
    use mtpool_core::{DnsState, Generation, RecordValue, Ts};
    let mut state = pool(vec![alpha()]);
    let value = RecordValue::A(vec![std::net::Ipv4Addr::new(203, 0, 113, 7)]);
    let entry = |in_flight| DnsState {
        desired: value.clone(),
        applied: Some(value.clone()),
        generation: Generation(1),
        retries: 3,
        next_try: Ts(NOW + 3_600_000),
        in_flight,
    };
    state.dns.insert("idle.example.com".into(), entry(None));
    state
        .dns
        .insert("busy.example.com".into(), entry(Some(Generation(1))));
    let env = env(state);
    let admin = admin_token(&env);

    let resp = post(&env, "/api/v1/admin/dns/push", &admin, None).await;
    assert_eq!(resp.status, StatusCode::ACCEPTED, "{}", resp.body);

    let (idle, busy) = env
        .actor
        .inspect(|s| {
            (
                s.dns["idle.example.com"].clone(),
                s.dns["busy.example.com"].clone(),
            )
        })
        .await
        .unwrap();
    assert_eq!(idle.applied, None);
    assert_eq!(idle.retries, 0);
    assert_eq!(idle.next_try, Ts(NOW));
    assert_eq!(busy, entry(Some(Generation(1))), "left alone");

    let entries = audit(&env, &admin).await;
    assert!(
        entries.iter().any(|e| e["action"] == "dns.push"),
        "{entries:?}"
    );
}

#[tokio::test]
async fn a_node_suspected_of_a_block_says_so_and_one_out_of_the_queue_does_not() {
    let mut suspected = alpha();
    suspected.state = NodeState::Quarantine;
    suspected.suspected = true;
    suspected.quarantine_reasons = vec![mtpool_core::FailedCondition::Globalping];
    let mut out_of_queue = beta();
    out_of_queue.state = NodeState::Quarantine;
    out_of_queue.quarantine_reasons = vec![mtpool_core::FailedCondition::Metrics];
    let env = env(pool(vec![suspected, out_of_queue]));
    let admin = admin_token(&env);

    for path in [
        "/api/v1/nodes/alpha-00001",
        "/api/v1/admin/nodes/alpha-00001",
    ] {
        let body = get(&env, path, &admin).await.json();
        assert_eq!(body["suspected"], true, "{path}: {body}");
        assert!(body["next_round_at"].is_string(), "{path}: {body}");
    }
    for path in ["/api/v1/nodes/beta-00002", "/api/v1/admin/nodes/beta-00002"] {
        let body = get(&env, path, &admin).await.json();
        assert!(body.get("suspected").is_none(), "{path}: {body}");
        // No sensors check it, so no round is due.
        assert!(body["next_round_at"].is_null(), "{path}: {body}");
    }
}
