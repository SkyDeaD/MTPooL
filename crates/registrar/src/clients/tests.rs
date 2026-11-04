use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use mtpool_core::{
    EngineKind, NodeId, NodeRecord, NodeState, NodeStats, OwnerId, PoolState, Settings, Ts,
};

use super::*;
use crate::actor::{Actor, ActorOptions};

const NOW: i64 = 1_800_000_000_000 + 12_345;

fn add(state: &mut PoolState, name: &str, node_state: NodeState, clients: u64) {
    let id = NodeId::parse(name).expect("valid node id");
    let mut node = NodeRecord::new(
        id.clone(),
        OwnerId(1),
        Ipv4Addr::new(203, 0, 113, 1),
        443,
        EngineKind::Telemt,
        Ts(0),
        &Settings::default(),
    );
    node.state = node_state;
    state.nodes.insert(id.clone(), node);
    state.stats.insert(
        id,
        NodeStats {
            clients: Some(clients),
            ..NodeStats::default()
        },
    );
}

fn samples(path: &std::path::Path) -> Vec<(i64, String, i64)> {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut stmt = conn
        .prepare("SELECT at, domain, clients FROM client_samples ORDER BY at, domain")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn samples_fall_on_the_30_second_grid() {
    assert_eq!(aligned(Ts(60_000)), Ts(60_000));
    assert_eq!(aligned(Ts(89_999)), Ts(60_000));
    assert_eq!(aligned(Ts(90_000)), Ts(90_000));
    assert_eq!(aligned(Ts(0)), Ts(0));
}

#[tokio::test]
async fn one_sample_is_queued_with_the_total_and_the_domains() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("registrar.db");
    let store = Store::open(&path).unwrap();
    let mut state = PoolState::new(Settings::default());
    state.settings.domains = vec!["a.example.com".to_owned(), "b.example.com".to_owned()];
    add(&mut state, "good-00001", NodeState::Healthy, 7);
    add(&mut state, "banned-00001", NodeState::Banned, 500);
    state
        .srmd
        .domain_clients
        .insert("a.example.com".to_owned(), 5);
    state
        .srmd
        .domain_clients
        .insert("b.example.com".to_owned(), 2);
    let clock_ms = Arc::new(AtomicI64::new(NOW));
    let shared = Arc::clone(&clock_ms);
    let clock: Clock = Arc::new(move || Ts(shared.load(Ordering::SeqCst)));
    let actor = Actor::spawn_with(
        state,
        store.clone(),
        Vec::new(),
        ActorOptions {
            clock: Arc::clone(&clock),
            ..ActorOptions::default()
        },
    );

    sample_once(&actor, &store, &clock).await.unwrap();
    store.flush().unwrap();

    let at = aligned(Ts(NOW)).0;
    assert_eq!(
        samples(&path),
        [
            (at, String::new(), 7),
            (at, "a.example.com".to_owned(), 5),
            (at, "b.example.com".to_owned(), 2),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn the_loop_samples_every_30_seconds_and_ends_with_the_owner() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("registrar.db");
    let store = Store::open(&path).unwrap();
    let mut state = PoolState::new(Settings::default());
    add(&mut state, "good-00001", NodeState::Healthy, 3);
    let clock_ms = Arc::new(AtomicI64::new(NOW));
    let shared = Arc::clone(&clock_ms);
    let clock: Clock = Arc::new(move || Ts(shared.load(Ordering::SeqCst)));
    let actor = Actor::spawn_with(
        state,
        store.clone(),
        Vec::new(),
        ActorOptions {
            clock: Arc::clone(&clock),
            ..ActorOptions::default()
        },
    );
    let worker = tokio::spawn(run(actor.clone(), store.clone(), clock));

    // The first tick is immediate; two more follow 30 s apart.
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        store.flush().unwrap();
        clock_ms.fetch_add(30_000, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
    store.flush().unwrap();
    let rows = samples(&path);
    assert!(rows.len() >= 3, "{rows:?}");
    assert!(
        rows.iter()
            .all(|(_, domain, clients)| domain.is_empty() && *clients == 3)
    );
    let times: Vec<i64> = rows.iter().map(|r| r.0).collect();
    assert!(times.windows(2).all(|w| w[1] - w[0] == 30_000), "{times:?}");
    worker.abort();
}

fn node_rows(path: &std::path::Path) -> Vec<(String, i64, i64, i64)> {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut stmt = conn
        .prepare("SELECT node, up, total, clients FROM node_samples ORDER BY node, bucket")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn spawn_actor(state: PoolState, store: &Store) -> (ActorHandle, Clock) {
    let clock: Clock = Arc::new(|| Ts(NOW));
    let actor = Actor::spawn_with(
        state,
        store.clone(),
        Vec::new(),
        ActorOptions {
            clock: Arc::clone(&clock),
            ..ActorOptions::default()
        },
    );
    (actor, clock)
}

#[tokio::test]
async fn a_tick_adds_the_nodes_to_their_bucket() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("registrar.db");
    let store = Store::open(&path).unwrap();
    let mut state = PoolState::new(Settings::default());
    add(&mut state, "good-00001", NodeState::Healthy, 7);
    add(&mut state, "quar-00001", NodeState::Quarantine, 1);
    add(&mut state, "pend-00001", NodeState::Pending, 1);
    add(&mut state, "banned-00001", NodeState::Banned, 500);
    let (actor, clock) = spawn_actor(state, &store);

    for _ in 0..2 {
        sample_once(&actor, &store, &clock).await.unwrap();
    }
    store.flush().unwrap();

    assert_eq!(
        node_rows(&path),
        [
            ("good-00001".to_owned(), 2, 2, 7),
            ("quar-00001".to_owned(), 0, 2, 1),
        ]
    );
}

#[tokio::test]
async fn the_refresh_hands_the_stored_availability_to_the_state_owner() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("registrar.db");
    let store = Store::open(&path).unwrap();
    let mut state = PoolState::new(Settings::default());
    add(&mut state, "good-00001", NodeState::Healthy, 7);
    add(&mut state, "fresh-00001", NodeState::Healthy, 7);
    let (actor, clock) = spawn_actor(state, &store);
    let at = aligned(Ts(NOW));
    for up in [true, true, true, false] {
        store.send(HistoryOp::NodeSamples {
            at,
            rows: vec![mtpool_core::NodeSample {
                node: NodeId::parse("good-00001").unwrap(),
                up,
                clients: None,
            }],
        });
    }
    // A node that is gone from the pool still has rows.
    store.send(HistoryOp::NodeSamples {
        at,
        rows: vec![mtpool_core::NodeSample {
            node: NodeId::parse("gone-00001").unwrap(),
            up: true,
            clients: None,
        }],
    });
    store.flush().unwrap();

    refresh_availability(&actor, &store, &clock).await.unwrap();

    let seen = actor
        .inspect(|state| {
            state
                .stats
                .iter()
                .map(|(id, s)| (id.to_string(), s.availability_bp))
                .collect::<Vec<_>>()
        })
        .await
        .unwrap();
    assert_eq!(
        seen,
        [
            ("fresh-00001".to_owned(), None),
            ("good-00001".to_owned(), Some(7500)),
        ]
    );
}

#[tokio::test]
async fn retention_removes_old_samples_and_keeps_fresh_ones() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("registrar.db");
    let store = Store::open(&path).unwrap();
    let bucket_now = NOW / 1000 / 900;
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        for (at, bucket) in [
            (NOW - 40 * 86_400_000, bucket_now - 40 * 96),
            (NOW, bucket_now),
        ] {
            conn.execute(
                "INSERT INTO client_samples(at, domain, clients) VALUES (?1, '', 1)",
                [at],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO node_samples(node, bucket, up, total, clients) VALUES ('alpha-00001', ?1, 1, 1, 0)",
                [bucket],
            )
            .unwrap();
        }
    }
    let clock: Clock = Arc::new(|| Ts(NOW));

    retain_once(&store, &clock).await;

    assert_eq!(samples(&path).len(), 1);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let nodes: i64 = conn
        .query_row("SELECT count(*) FROM node_samples", [], |r| r.get(0))
        .unwrap();
    assert_eq!(nodes, 1);
}
