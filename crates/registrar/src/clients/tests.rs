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
