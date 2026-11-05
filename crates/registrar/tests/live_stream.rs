//! The live numbers of the public pages: `GET /api/v1/stream`.

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use common::http::*;
use http_body_util::BodyExt;
use mtpool_core::{NodeId, NodeState, NodeStats, PoolState};
use mtpool_registrar::api::live::LiveOptions;
use mtpool_registrar::auth::tokens::TokenKind;
use serde_json::Value;
use tower::ServiceExt;

const PATH: &str = "/api/v1/stream";
const FIRST_IP: &str = "45.33.1.5";

fn fast() -> LiveOptions {
    LiveOptions {
        clients_every: Duration::from_millis(40),
        changed_every: Duration::from_millis(10),
        keepalive: Duration::from_secs(15),
        ..LiveOptions::default()
    }
}

fn stats(clients: u64, other: Option<u64>) -> NodeStats {
    NodeStats {
        clients: Some(clients),
        other_clients: other,
        ..NodeStats::default()
    }
}

/// Three nodes that count and one that does not: its clients are gone with its ban.
fn busy_pool() -> PoolState {
    let nodes = [
        (
            "alpha-00001",
            [45, 33, 1, 5],
            NodeState::Healthy,
            7,
            Some(40),
        ),
        (
            "beta-00002",
            [45, 33, 1, 6],
            NodeState::Quarantine,
            2,
            Some(0),
        ),
        ("gamma-00003", [45, 33, 1, 7], NodeState::Pending, 1, None),
        (
            "delta-00004",
            [45, 33, 1, 8],
            NodeState::Banned,
            90,
            Some(90),
        ),
    ];
    let mut state = pool(
        nodes
            .iter()
            .map(|(id, ip, kind, _, _)| node_at(id, *ip, 1, *kind))
            .collect(),
    );
    for (id, _, _, clients, other) in nodes {
        state
            .stats
            .insert(NodeId::parse(id).unwrap(), stats(clients, other));
    }
    state
}

async fn open(env: &Env) -> axum::response::Response {
    env.app
        .clone()
        .oneshot(request("GET", PATH, None, None))
        .await
        .unwrap()
}

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

fn clients_frames(seen: &str) -> Vec<Value> {
    seen.split("\n\n")
        .filter(|frame| frame.starts_with("event: clients\n"))
        .map(|frame| {
            let data = frame
                .lines()
                .nth(1)
                .unwrap()
                .strip_prefix("data: ")
                .unwrap();
            serde_json::from_str(data).unwrap()
        })
        .collect()
}

async fn ended(body: &mut axum::body::Body) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match tokio::time::timeout_at(deadline, body.frame()).await {
            Err(_) => panic!("the stream is still open"),
            Ok(None) => return,
            Ok(Some(_)) => {}
        }
    }
}

#[tokio::test]
async fn anyone_gets_the_numbers_at_once_and_no_address() {
    // The ticks of the worker are not running here: the first frame comes from the request.
    let env = env(busy_pool());
    let response = open(&env).await;
    assert_eq!(response.status(), StatusCode::OK);
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned()
    };
    assert!(header("content-type").starts_with("text/event-stream"));
    assert_eq!(header("cache-control"), "no-cache, no-transform");
    assert_eq!(header("x-accel-buffering"), "no");

    let mut body = response.into_body();
    let mut seen = String::new();
    pump(&mut body, &mut seen, "event: clients\n").await;
    pump(&mut body, &mut seen, "}\n\n").await;
    let tick = &clients_frames(&seen)[0];
    assert_eq!(tick["total"], 10);
    assert!(tick["at"].as_str().unwrap().ends_with('Z'), "{tick}");
    assert_eq!(tick["nodes"]["alpha-00001"], 7);
    assert_eq!(tick["nodes"]["beta-00002"], 2);
    assert_eq!(tick["nodes"]["gamma-00003"], 1);
    assert!(tick["nodes"].get("delta-00004").is_none(), "{tick}");
    // Only the numbers that are there, and only of the nodes that count.
    assert_eq!(tick["other_nodes"]["alpha-00001"], 40);
    assert_eq!(tick["other_nodes"].as_object().unwrap().len(), 1, "{tick}");
    assert!(!seen.contains(FIRST_IP), "{seen}");
}

#[tokio::test]
async fn the_numbers_keep_coming() {
    let env = env_with_live(busy_pool(), fast());
    let mut body = open(&env).await.into_body();
    let mut seen = String::new();
    pump(&mut body, &mut seen, "event: clients\n").await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while clients_frames(&seen).len() < 3 {
        let frame = tokio::time::timeout_at(deadline, body.frame())
            .await
            .expect("no more ticks")
            .expect("the stream ended")
            .expect("frame");
        if let Ok(data) = frame.into_data() {
            seen.push_str(std::str::from_utf8(&data).unwrap());
        }
    }
    assert_eq!(clients_frames(&seen)[2]["total"], 10);
}

#[tokio::test]
async fn a_change_of_the_pool_is_announced_once() {
    let env = env_with_live(busy_pool(), fast());
    let admin = token(&env, TokenKind::Admin, None);
    let mut body = open(&env).await.into_body();
    let mut seen = String::new();
    pump(&mut body, &mut seen, "event: clients\n").await;
    assert!(!seen.contains("event: changed"), "{seen}");

    let retire = call(
        &env,
        "POST",
        "/api/v1/admin/nodes/alpha-00001/retire",
        &admin,
        None,
    )
    .await;
    assert_eq!(retire.status, StatusCode::OK, "{}", retire.body);
    pump(&mut body, &mut seen, "event: changed\ndata: {}\n\n").await;

    // Once for one change, whatever time passes after it.
    tokio::time::sleep(Duration::from_millis(120)).await;
    let wait = tokio::time::timeout(Duration::from_millis(50), async {
        while let Some(Ok(frame)) = body.frame().await {
            if let Ok(data) = frame.into_data() {
                seen.push_str(std::str::from_utf8(&data).unwrap());
            }
        }
    });
    let _ = wait.await;
    assert_eq!(seen.matches("event: changed").count(), 1, "{seen}");
    // And the numbers follow: the dead node is not counted any more.
    let last = clients_frames(&seen).pop().unwrap();
    assert_eq!(last["total"], 3, "{last}");
    // The event itself stays in the panel's stream: nothing about it goes out here.
    assert!(!seen.contains("node_dead"), "{seen}");
}

#[tokio::test]
async fn too_many_streams_of_one_address_are_refused_until_one_closes() {
    let env = env_with_live(
        busy_pool(),
        LiveOptions {
            max_per_address: 1,
            ..fast()
        },
    );
    let first = open(&env).await;
    assert_eq!(first.status(), StatusCode::OK);

    let second = open(&env).await;
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(second.headers().contains_key("retry-after"));

    drop(first);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if open(&env).await.status() == StatusCode::OK {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the closed stream still holds its place"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn the_streams_of_the_whole_service_are_limited() {
    let env = env_with_live(
        busy_pool(),
        LiveOptions {
            max_streams: 1,
            ..fast()
        },
    );
    let _first = open(&env).await;
    let second = open(&env).await;
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn the_stream_ends_with_the_state_owner() {
    let env = env_with_live(busy_pool(), fast());
    let mut body = open(&env).await.into_body();
    let mut seen = String::new();
    pump(&mut body, &mut seen, "event: clients\n").await;
    env.actor.stop().await;
    ended(&mut body).await;
}

#[tokio::test]
async fn closing_ends_the_streams_and_refuses_new_ones() {
    let env = env_with_live(busy_pool(), fast());
    let mut body = open(&env).await.into_body();
    let mut seen = String::new();
    pump(&mut body, &mut seen, "event: clients\n").await;
    env.live.close();
    ended(&mut body).await;
    let mut after = open(&env).await.into_body();
    ended(&mut after).await;
}

/// The wait the stream tells the browser to keep before it reconnects, in milliseconds.
async fn retry_of(response: axum::response::Response) -> u64 {
    let mut body = response.into_body();
    let mut seen = String::new();
    pump(&mut body, &mut seen, "\n\n").await;
    let line = seen.lines().next().unwrap();
    line.strip_prefix("retry: ")
        .unwrap_or_else(|| panic!("the stream does not start with its wait: {seen:?}"))
        .parse()
        .unwrap()
}

#[tokio::test]
async fn each_stream_gets_its_own_wait_before_it_reconnects() {
    let env = env(busy_pool());
    let mut waits = Vec::new();
    for _ in 0..12 {
        waits.push(retry_of(open(&env).await).await);
    }
    for wait in &waits {
        assert!((5_000..=15_000).contains(wait), "{waits:?}");
    }
    // Tabs that lost one service must not come back at one moment.
    waits.sort_unstable();
    waits.dedup();
    assert!(waits.len() > 1, "every stream got the same wait: {waits:?}");
}

#[tokio::test]
async fn a_stream_ends_by_itself_when_it_is_old() {
    let env = env_with_live(
        busy_pool(),
        LiveOptions {
            max_age: Duration::from_millis(300),
            ..fast()
        },
    );
    let opened = tokio::time::Instant::now();
    let mut body = open(&env).await.into_body();
    ended(&mut body).await;
    let lived = opened.elapsed();
    // A client that keeps reading is still sent away, and not before half of the age.
    assert!(lived >= Duration::from_millis(150), "{lived:?}");
}

#[tokio::test]
async fn an_old_stream_gives_its_place_back() {
    let env = env_with_live(
        busy_pool(),
        LiveOptions {
            max_per_address: 1,
            max_age: Duration::from_millis(200),
            ..fast()
        },
    );
    let mut first = open(&env).await.into_body();
    assert_eq!(open(&env).await.status(), StatusCode::TOO_MANY_REQUESTS);
    ended(&mut first).await;
    // The body is still held: only the age could have freed the place.
    assert_eq!(open(&env).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn streams_opened_together_do_not_end_together() {
    let env = env_with_live(
        busy_pool(),
        LiveOptions {
            max_age: Duration::from_millis(800),
            ..fast()
        },
    );
    let opened = tokio::time::Instant::now();
    let mut streams = tokio::task::JoinSet::new();
    for _ in 0..12 {
        let mut body = open(&env).await.into_body();
        streams.spawn(async move {
            ended(&mut body).await;
            opened.elapsed()
        });
    }
    let lives = streams.join_all().await;
    let (first, last) = (lives.iter().min().unwrap(), lives.iter().max().unwrap());
    // After a restart the tabs come back within seconds of each other. They must not all leave
    // again together when their age runs out.
    assert!(
        *last - *first >= Duration::from_millis(50),
        "they ended together: {lives:?}"
    );
}

#[tokio::test]
async fn the_stream_of_the_panel_has_no_age_and_keeps_its_wait() {
    let env = env_with_live(
        busy_pool(),
        LiveOptions {
            max_age: Duration::from_millis(50),
            ..fast()
        },
    );
    let admin = token(&env, TokenKind::Admin, None);
    let response = env
        .app
        .clone()
        .oneshot(request("GET", "/api/v1/admin/stream", Some(&admin), None))
        .await
        .unwrap();
    let mut body = response.into_body();
    let mut seen = String::new();
    pump(&mut body, &mut seen, "retry: 5000\n\n").await;
    let more = tokio::time::timeout(Duration::from_millis(300), body.frame()).await;
    assert!(more.is_err(), "the stream of the panel ended: {more:?}");
}
