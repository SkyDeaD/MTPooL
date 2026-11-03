//! Shared fixtures of the integration tests.
#![allow(dead_code)]

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;

use mtpool_core::{
    Command, EngineKind, EnrollRequest, GpStatus, HeartbeatRequest, NodeId, NodeRecord, NodeState,
    OwnerId, PoolState, ReplyId, Settings, Ts,
};
use mtpool_registrar::actor::Clock;
use mtpool_registrar::store::Store;

pub fn node_id(name: &str) -> NodeId {
    NodeId::parse(name).expect("valid node id")
}

pub fn temp_db() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("registrar.db");
    (dir, path)
}

pub fn open_store() -> (tempfile::TempDir, Store) {
    let (dir, path) = temp_db();
    let store = Store::open(&path).expect("open store");
    (dir, store)
}

pub fn enroll(name: &str, ip: [u8; 4]) -> Command {
    Command::Enroll(EnrollRequest {
        reply: ReplyId(0),
        node_id: node_id(name),
        owner: OwnerId(1),
        ip: Ipv4Addr::from(ip),
        port: 443,
        engine: EngineKind::Telemt,
    })
}

pub fn heartbeat(name: &str, ip: [u8; 4]) -> Command {
    Command::Heartbeat(HeartbeatRequest {
        reply: ReplyId(0),
        node: node_id(name),
        ip: Ipv4Addr::from(ip),
    })
}

/// A heartbeat of a node nobody registered: a cheap round trip through the actor.
pub fn barrier() -> Command {
    heartbeat("nobody-00000", [45, 33, 9, 9])
}

/// A healthy node whose last heartbeat is `heartbeat_age_ms` old at `now`.
pub fn healthy_node(name: &str, now: Ts, heartbeat_age_ms: i64) -> NodeRecord {
    let settings = Settings::default();
    let mut node = NodeRecord::new(
        node_id(name),
        OwnerId(1),
        Ipv4Addr::new(45, 33, 1, 5),
        443,
        EngineKind::Telemt,
        now,
        &settings,
    );
    node.state = NodeState::Healthy;
    node.health.tcp.up = true;
    node.health.metrics.up = true;
    node.health.gp = GpStatus::Ok { at: now };
    node.health.last_report = now;
    node.health.last_heartbeat = Ts(now.0 - heartbeat_age_ms);
    node
}

pub fn state_with(node: NodeRecord, settings: Settings) -> PoolState {
    let mut state = PoolState::new(settings);
    state.nodes.insert(node.id.clone(), node);
    state
}

pub fn fixed_clock(ms: i64) -> Clock {
    Arc::new(move || Ts(ms))
}
