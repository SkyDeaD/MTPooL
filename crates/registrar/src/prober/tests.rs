use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use mtpool_core::{
    EngineKind, Generation, NodeId, NodeRecord, NodeState, OwnerId, PoolState, Settings, Ts,
};
use tokio::net::TcpListener;
use tokio::time::Instant;

use super::*;
use crate::actor::{Actor, ActorOptions};
use crate::store::Store;

fn id(name: &str) -> NodeId {
    NodeId::parse(name).expect("valid node id")
}

fn target(n: usize) -> Target {
    Target {
        node: id(&format!("node-{n:05}")),
        generation: Generation(0),
        addr: SocketAddr::from(([203, 0, 113, 1], 443)),
    }
}

/// Counts the connections in flight and remembers the most there ever were.
#[derive(Default)]
struct Counting {
    now: AtomicUsize,
    most: AtomicUsize,
}

impl Connector for Counting {
    fn connect(&self, _addr: SocketAddr) -> ConnectFuture<'_> {
        Box::pin(async move {
            let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
            self.most.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(100)).await;
            self.now.fetch_sub(1, Ordering::SeqCst);
            true
        })
    }
}

/// Answers after `delay`.
struct After(Duration);

impl Connector for After {
    fn connect(&self, _addr: SocketAddr) -> ConnectFuture<'_> {
        Box::pin(async move {
            tokio::time::sleep(self.0).await;
            true
        })
    }
}

/// Never answers.
struct Silent;

impl Connector for Silent {
    fn connect(&self, _addr: SocketAddr) -> ConnectFuture<'_> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test(start_paused = true)]
async fn prober_concurrency_bounded() {
    let counting = Arc::new(Counting::default());
    let connector: Arc<dyn Connector> = counting.clone();
    let targets: Vec<Target> = (0..200).map(target).collect();
    let results = probe_all(&connector, targets, Duration::from_secs(3), MAX_CONCURRENT).await;
    assert_eq!(results.len(), 200);
    assert!(results.iter().all(|(_, _, open)| *open));
    // The bound is reached (the work is not serialized) and never passed.
    assert_eq!(counting.most.load(Ordering::SeqCst), MAX_CONCURRENT);
    assert_eq!(counting.now.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn prober_timeout_3s() {
    let silent: Arc<dyn Connector> = Arc::new(Silent);
    let started = Instant::now();
    let results = probe_all(&silent, vec![target(1)], Duration::from_secs(3), 64).await;
    assert_eq!(started.elapsed(), Duration::from_secs(3));
    assert_eq!(results, vec![(id("node-00001"), Generation(0), false)]);

    // Just in time is open, just late is closed.
    let in_time: Arc<dyn Connector> = Arc::new(After(Duration::from_millis(2900)));
    let results = probe_all(&in_time, vec![target(2)], Duration::from_secs(3), 64).await;
    assert!(results[0].2);
    let late: Arc<dyn Connector> = Arc::new(After(Duration::from_millis(3100)));
    let results = probe_all(&late, vec![target(3)], Duration::from_secs(3), 64).await;
    assert!(!results[0].2);
}

#[tokio::test]
async fn results_keep_the_generation_of_the_target() {
    let connector: Arc<dyn Connector> = Arc::new(After(Duration::ZERO));
    let mut t = target(7);
    t.generation = Generation(4);
    let results = probe_all(&connector, vec![t], Duration::from_secs(3), 64).await;
    assert_eq!(results, vec![(id("node-00007"), Generation(4), true)]);
}

#[tokio::test]
async fn tcp_connector_sees_an_open_and_a_closed_port() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let open = listener.local_addr().unwrap();
    let closed = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    assert!(TcpConnector.connect(open).await);
    assert!(!TcpConnector.connect(closed).await);
}

fn node(name: &str, port: u16, state: NodeState, tcp_up: bool) -> NodeRecord {
    let mut n = NodeRecord::new(
        id(name),
        OwnerId(1),
        Ipv4Addr::LOCALHOST,
        port,
        EngineKind::Telemt,
        Ts(0),
        &Settings::default(),
    );
    n.state = state;
    n.health.tcp.up = tcp_up;
    n
}

/// The loop reads the live nodes from the owner and its answers change their latches: a node
/// that accepts comes up, one that does not goes down, and a banned node is left alone.
#[tokio::test]
async fn loop_reports_to_the_state_owner() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("registrar.db")).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let open_port = listener.local_addr().unwrap().port();
    let accept = tokio::spawn(async move {
        loop {
            let _ = listener.accept().await;
        }
    });
    let closed_port = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    };
    let mut state = PoolState::new(Settings::default());
    for n in [
        node("opens-00001", open_port, NodeState::Pending, false),
        node("closes-00001", closed_port, NodeState::Healthy, true),
        node("banned-00001", closed_port, NodeState::Banned, true),
    ] {
        state.nodes.insert(n.id.clone(), n);
    }
    let actor = Actor::spawn_with(state, store, Vec::new(), ActorOptions::default());
    let prober = tokio::spawn(
        Prober::new()
            .with_interval(Duration::from_millis(20))
            .run(actor.clone()),
    );
    let started = std::time::Instant::now();
    loop {
        let (opens, closes, banned) = actor
            .inspect(|s| {
                (
                    s.nodes[&id("opens-00001")].health.tcp.up,
                    s.nodes[&id("closes-00001")].health.tcp.up,
                    s.nodes[&id("banned-00001")].health.tcp.up,
                )
            })
            .await
            .unwrap();
        if opens && !closes {
            assert!(banned, "a node that is not live is not probed");
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(10), "no result");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    actor.stop().await;
    // The loop ends with the owner.
    tokio::time::timeout(Duration::from_secs(5), prober)
        .await
        .expect("the prober stops")
        .unwrap();
    accept.abort();
}
