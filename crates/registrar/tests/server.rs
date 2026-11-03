mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::*;
use mtpool_core::{Event, EventKind, PoolState, Ts};
use mtpool_registrar::actor::Actor;
use mtpool_registrar::server::{AppState, router, shutdown};
use mtpool_registrar::store::{Store, WriteHealth};
use tower::ServiceExt;

fn app(store: Store) -> (axum::Router, mtpool_registrar::actor::ActorHandle) {
    let actor = Actor::spawn(PoolState::default(), store.clone(), vec![]);
    let state = AppState {
        store,
        actor: actor.clone(),
        snapshots: actor.snapshots(),
    };
    (router(state), actor)
}

async fn status(app: &axum::Router, request: Request<Body>) -> StatusCode {
    app.clone()
        .oneshot(request)
        .await
        .expect("response")
        .status()
}

fn get(path: &str) -> Request<Body> {
    Request::builder().uri(path).body(Body::empty()).unwrap()
}

#[tokio::test]
async fn healthz_200_always() {
    let (_dir, store) = open_store();
    let (app, _actor) = app(store.clone());
    assert_eq!(status(&app, get("/healthz")).await, StatusCode::OK);

    // Still 200 when the store is broken: liveness says only that the process answers.
    store.close().unwrap();
    store.send(Event {
        kind: EventKind::StoreError,
        at: Ts(1),
        node: None,
        domain: None,
        detail: serde_json::Value::Null,
    });
    assert!(matches!(store.health(), WriteHealth::Failed { .. }));
    assert_eq!(status(&app, get("/healthz")).await, StatusCode::OK);
}

#[tokio::test]
async fn readyz_503_when_store_unhealthy() {
    let (_dir, store) = open_store();
    let (app, _actor) = app(store.clone());
    assert_eq!(status(&app, get("/readyz")).await, StatusCode::OK);

    store.close().unwrap();
    store.send(Event {
        kind: EventKind::StoreError,
        at: Ts(1),
        node: None,
        domain: None,
        detail: serde_json::Value::Null,
    });
    assert_eq!(
        status(&app, get("/readyz")).await,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn request_body_over_256kib_rejected() {
    let (_dir, store) = open_store();
    let (app, _actor) = app(store);
    let limit = 256 * 1024;
    let post = |path: &str, len: usize| {
        Request::builder()
            .method("POST")
            .uri(path)
            .header(header::CONTENT_LENGTH, len)
            .body(Body::from(vec![b'x'; len]))
            .unwrap()
    };
    for path in ["/healthz", "/readyz", "/no/such/route"] {
        assert_eq!(
            status(&app, post(path, limit + 1)).await,
            StatusCode::PAYLOAD_TOO_LARGE,
            "{path}"
        );
    }
    // Exactly at the limit the body is not what stops the request.
    assert_ne!(
        status(&app, post("/healthz", limit)).await,
        StatusCode::PAYLOAD_TOO_LARGE
    );
}

#[tokio::test]
async fn graceful_shutdown_flushes() {
    let (_dir, path) = temp_db();
    // The batch interval is an hour: a heartbeat reaches the disk only through the shutdown.
    let store = Store::open_with(
        &path,
        mtpool_registrar::store::StoreOptions {
            batch_interval: std::time::Duration::from_secs(3600),
        },
    )
    .unwrap();
    let clock = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(1_000_000));
    let clock_for_actor = std::sync::Arc::clone(&clock);
    let actor = Actor::spawn_with(
        PoolState::default(),
        store.clone(),
        vec![],
        mtpool_registrar::actor::ActorOptions {
            clock: std::sync::Arc::new(move || {
                Ts(clock_for_actor.load(std::sync::atomic::Ordering::SeqCst))
            }),
            ..Default::default()
        },
    );
    actor
        .call(enroll("alpha-00001", [45, 33, 1, 5]))
        .await
        .unwrap();
    clock.store(5_000_000, std::sync::atomic::Ordering::SeqCst);
    actor
        .call(heartbeat("alpha-00001", [45, 33, 1, 5]))
        .await
        .unwrap();

    shutdown(&actor, &store).await.expect("shutdown");

    let reopened = Store::open(&path).unwrap();
    let state = reopened.load().unwrap();
    let node = state
        .nodes
        .get(&node_id("alpha-00001"))
        .expect("node stored");
    assert_eq!(node.health.last_heartbeat, Ts(5_000_000));
}
