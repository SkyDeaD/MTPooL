use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mtpool_core::{
    AbstainReason, Assignments, DnsState, EngineKind, Event, EventKind, Generation, GpStatus,
    IpBlock, NodeHealth, NodeId, NodeRecord, NodeSample, NodeState, NodeStats, OwnerId, PersistOp,
    PoolState, RecordValue, RoundVerdict, SensorKind, SensorVote, Settings, Slot, SrmdState,
    Tombstone, TrafficCounters, Ts, Vote,
};
use rusqlite::Connection;
use serde_json::json;
use tempfile::TempDir;

use super::*;

const DAY_MS: i64 = 24 * 3600 * 1000;
const NOW: Ts = Ts(1_800_000_000_000);

fn temp_db() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pool.db");
    (dir, path)
}

fn fresh(path: &Path) -> Connection {
    Connection::open(path).unwrap()
}

fn count(path: &Path, sql: &str) -> i64 {
    fresh(path).query_row(sql, [], |r| r.get(0)).unwrap()
}

fn id(s: &str) -> NodeId {
    NodeId::parse(s).unwrap()
}

fn ip(s: &str) -> Ipv4Addr {
    s.parse().unwrap()
}

fn node(name: &str, addr: &str, state: NodeState) -> NodeRecord {
    let mut n = NodeRecord::new(
        id(name),
        OwnerId(7),
        ip(addr),
        443,
        EngineKind::Telemt,
        Ts(NOW.0 - 3 * DAY_MS),
        &Settings::default(),
    );
    n.state = state;
    n.state_since = Ts(NOW.0 - DAY_MS);
    n
}

fn event(kind: EventKind, at: Ts) -> Event {
    Event {
        kind,
        at,
        node: Some(id("alpha-a1b2c")),
        owner: None,
        domain: None,
        detail: json!({ "k": "v" }),
    }
}

fn traffic(at: Ts) -> PersistOp {
    PersistOp::Traffic {
        node: id("alpha-a1b2c"),
        at,
        from_client: 10,
        to_client: 20,
        other_from_client: 30,
        other_to_client: 40,
    }
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn migrations_apply_on_empty_db() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();

    let conn = store.read();
    for table in [
        "schema_migrations",
        "settings",
        "nodes",
        "node_stats",
        "sensor_rounds",
        "sensor_votes",
        "bans",
        "ip_blocks",
        "tombstones",
        "assignments",
        "dns_records",
        "srmd_state",
        "events",
        "traffic",
        "owners",
        "tokens",
        "sessions",
        "gp_measurements",
        "client_samples",
        "node_samples",
    ] {
        let found: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(found, 1, "table {table} is missing");
    }
    let version: i64 = conn
        .query_row("SELECT max(version) FROM schema_migrations", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(version, 10);
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    drop(conn);

    assert_eq!(
        store.load_at(NOW).unwrap(),
        PoolState::new(Settings::default())
    );
    assert_eq!(store.health(), WriteHealth::Ok);
    store.close().unwrap();

    // Opening again applies nothing twice.
    let store = Store::open(&path).unwrap();
    store.close().unwrap();
    assert_eq!(count(&path, "SELECT count(*) FROM schema_migrations"), 10);
}

#[test]
fn the_stored_two_clean_rounds_become_one() {
    for (stored, expected) in [(2, 1), (3, 3)] {
        let (_dir, path) = temp_db();
        database_at(&path, 9);
        let json = serde_json::to_string(&Settings {
            clean_rounds_to_exit: stored,
            proxy_port: 8443,
            ..Settings::default()
        })
        .unwrap();
        fresh(&path)
            .execute(
                "INSERT INTO settings(id, json) VALUES (1, ?1) \
                 ON CONFLICT(id) DO UPDATE SET json = excluded.json",
                [json],
            )
            .unwrap();

        let store = Store::open(&path).unwrap();
        let settings = store.load_at(NOW).unwrap().settings;
        assert_eq!(settings.clean_rounds_to_exit, expected);
        assert_eq!(settings.proxy_port, 8443);
        store.close().unwrap();
        assert_eq!(version_of(&path), 10);
    }
}

#[test]
fn a_database_at_version_5_upgrades_cleanly() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    store.send(PersistOp::SetSettings(Settings {
        proxy_port: 8443,
        ..Settings::default()
    }));
    store.flush().unwrap();
    store.close().unwrap();
    // Take the database back to what version 5 left.
    let conn = fresh(&path);
    conn.execute_batch(
        "DROP TABLE client_samples; DROP TABLE node_samples; \
         ALTER TABLE traffic DROP COLUMN other_from_client; ALTER TABLE traffic DROP COLUMN other_to_client; \
         ALTER TABLE bans DROP COLUMN node_registered_at; ALTER TABLE ip_blocks DROP COLUMN owner; DROP INDEX tokens_node_id; \
         DELETE FROM schema_migrations WHERE version >= 6;",
    )
    .unwrap();
    drop(conn);

    let store = Store::open(&path).unwrap();
    assert_eq!(store.load_at(NOW).unwrap().settings.proxy_port, 8443);
    store.close().unwrap();
    assert_eq!(
        count(&path, "SELECT max(version) FROM schema_migrations"),
        10
    );
    assert_eq!(count(&path, "SELECT count(*) FROM client_samples"), 0);
}

#[test]
fn a_database_at_version_7_upgrades_and_loses_the_tokens_of_nodes_it_no_longer_has() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    store.send(PersistOp::UpsertNode(node(
        "kept-a1b2c",
        "198.51.100.1",
        NodeState::Healthy,
    )));
    store.flush().unwrap();
    store.close().unwrap();
    let conn = fresh(&path);
    conn.execute_batch(
        "ALTER TABLE traffic DROP COLUMN other_from_client; ALTER TABLE traffic DROP COLUMN other_to_client; \
         ALTER TABLE bans DROP COLUMN node_registered_at; ALTER TABLE ip_blocks DROP COLUMN owner; DROP INDEX tokens_node_id; \
         DELETE FROM schema_migrations WHERE version >= 8; \
         INSERT INTO ip_blocks(ip, node_id, blocked_at) VALUES ('198.51.100.9', 'old-a1b2c', 1); \
         INSERT INTO tokens(kind, hash, prefix, node_id, created_at) \
           VALUES ('node', x'01', 'p', 'kept-a1b2c', 1), ('node', x'02', 'p', 'gone-a1b2c', 1); \
         INSERT INTO tokens(kind, hash, prefix, created_at) VALUES ('metrics', x'03', 'p', 1);",
    )
    .unwrap();
    drop(conn);

    let store = Store::open(&path).unwrap();
    let state = store.load_at(NOW).unwrap();
    assert_eq!(state.blocks[0].node_id, id("old-a1b2c"));
    assert_eq!(
        state.blocks[0].owner, None,
        "a block from before the column"
    );
    store.close().unwrap();
    assert_eq!(
        count(
            &path,
            "SELECT count(*) FROM tokens WHERE node_id = 'kept-a1b2c'"
        ),
        1
    );
    assert_eq!(
        count(
            &path,
            "SELECT count(*) FROM tokens WHERE node_id = 'gone-a1b2c'"
        ),
        0
    );
    assert_eq!(
        count(&path, "SELECT count(*) FROM tokens WHERE kind = 'metrics'"),
        1
    );
}

#[test]
fn deleting_a_node_deletes_its_token_and_keeps_its_history() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    store.send(PersistOp::UpsertNode(node(
        "gone-a1b2c",
        "198.51.100.1",
        NodeState::Healthy,
    )));
    store.send(PersistOp::UpsertNode(node(
        "kept-a1b2c",
        "198.51.100.2",
        NodeState::Healthy,
    )));
    store.send(PersistOp::RecordBan {
        ip: ip("198.51.100.1"),
        node: id("gone-a1b2c"),
        at: NOW,
        votes: vec![],
        reverify: false,
        node_registered_at: None,
    });
    store.flush().unwrap();
    let conn = fresh(&path);
    conn.execute_batch(
        "INSERT INTO tokens(kind, hash, prefix, node_id, created_at) \
           VALUES ('node', x'01', 'p', 'gone-a1b2c', 1), ('node', x'02', 'p', 'kept-a1b2c', 1); \
         INSERT INTO sensor_rounds(node, generation, at, verdict) \
           VALUES ('gone-a1b2c', 0, 1, 'confirmed'); \
         INSERT INTO node_samples(node, bucket, up, total) VALUES ('gone-a1b2c', 1, 1, 1);",
    )
    .unwrap();
    drop(conn);

    store.send(PersistOp::DeleteNode(id("gone-a1b2c")));
    store.flush().unwrap();
    store.close().unwrap();

    assert_eq!(
        count(&path, "SELECT count(*) FROM nodes WHERE id = 'gone-a1b2c'"),
        0
    );
    assert_eq!(
        count(
            &path,
            "SELECT count(*) FROM tokens WHERE node_id = 'gone-a1b2c'"
        ),
        0
    );
    assert_eq!(
        count(
            &path,
            "SELECT count(*) FROM tokens WHERE node_id = 'kept-a1b2c'"
        ),
        1
    );
    assert_eq!(
        count(&path, "SELECT count(*) FROM bans WHERE node = 'gone-a1b2c'"),
        1
    );
    assert_eq!(
        count(
            &path,
            "SELECT count(*) FROM sensor_rounds WHERE node = 'gone-a1b2c'"
        ),
        1
    );
    assert_eq!(
        count(
            &path,
            "SELECT count(*) FROM node_samples WHERE node = 'gone-a1b2c'"
        ),
        1
    );
}

#[test]
fn a_database_at_version_6_upgrades_cleanly() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    store.send(PersistOp::SetSettings(Settings {
        proxy_port: 8443,
        ..Settings::default()
    }));
    store.flush().unwrap();
    store.close().unwrap();
    let conn = fresh(&path);
    conn.execute_batch(
        "DROP TABLE node_samples; \
         ALTER TABLE traffic DROP COLUMN other_from_client; ALTER TABLE traffic DROP COLUMN other_to_client; \
         ALTER TABLE bans DROP COLUMN node_registered_at; ALTER TABLE ip_blocks DROP COLUMN owner; \
         DROP INDEX tokens_node_id; DELETE FROM schema_migrations WHERE version >= 7;",
    )
    .unwrap();
    drop(conn);

    let store = Store::open(&path).unwrap();
    assert_eq!(store.load_at(NOW).unwrap().settings.proxy_port, 8443);
    store.close().unwrap();
    assert_eq!(
        count(&path, "SELECT max(version) FROM schema_migrations"),
        10
    );
    assert_eq!(count(&path, "SELECT count(*) FROM node_samples"), 0);
}

fn sample(name: &str, up: bool, clients: Option<u64>) -> NodeSample {
    NodeSample {
        node: id(name),
        up,
        clients,
    }
}

#[test]
fn node_samples_accumulate_in_a_bucket_and_split_across_buckets() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    let bucket_ms = BUCKET_SEC * 1000;
    let start = NOW.0 - NOW.0 % bucket_ms;
    for (offset, up, clients) in [
        (0, true, Some(5)),
        (30_000, true, Some(9)),
        (60_000, false, None),
        (bucket_ms, true, Some(2)),
    ] {
        store.send(HistoryOp::NodeSamples {
            at: Ts(start + offset),
            rows: vec![sample("alpha-00001", up, clients)],
        });
    }
    store.flush().unwrap();

    let now = Ts(start + bucket_ms + 1000);
    let rows = node_history(&store.read(), &id("alpha-00001"), now, 96).unwrap();
    assert_eq!(
        rows,
        [
            NodeBucket {
                start: Ts(start),
                up: 2,
                total: 3,
                clients: 9
            },
            NodeBucket {
                start: Ts(start + bucket_ms),
                up: 1,
                total: 1,
                clients: 2
            },
        ]
    );
    // Only the last buckets asked for.
    let last = node_history(&store.read(), &id("alpha-00001"), now, 1).unwrap();
    assert_eq!(last.len(), 1);
    assert_eq!(last[0].start, Ts(start + bucket_ms));
    // Another node has no history.
    assert!(
        node_history(&store.read(), &id("beta-00002"), now, 96)
            .unwrap()
            .is_empty()
    );
    store.close().unwrap();
}

#[test]
fn availability_is_up_over_total_in_30_days_and_none_without_samples() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    let conn = fresh(&path);
    let bucket_now = NOW.0 / 1000 / BUCKET_SEC;
    let insert = |node: &str, bucket: i64, up: i64, total: i64| {
        conn.execute(
            "INSERT INTO node_samples(node, bucket, up, total, clients) VALUES (?1, ?2, ?3, ?4, 0)",
            rusqlite::params![node, bucket, up, total],
        )
        .unwrap();
    };
    // 3 of 10000 ticks down: 99.97 %.
    insert("alpha-00001", bucket_now, 4997, 5000);
    insert("alpha-00001", bucket_now - 100, 5000, 5000);
    // Older than 30 days: not counted.
    insert("alpha-00001", bucket_now - 30 * 96, 0, 5000);
    // Always down: 0, which is a value, unlike no samples.
    insert("beta-00002", bucket_now, 0, 10);
    drop(conn);

    let mut list = availability(&store.read(), NOW).unwrap();
    list.sort();
    assert_eq!(list, [(id("alpha-00001"), 9997), (id("beta-00002"), 0)]);
    // A node with no samples is not in the list at all.
    assert!(list.iter().all(|(n, _)| n != &id("gamma-00003")));
    // Far in the future every sample is out of the window.
    assert!(
        availability(&store.read(), Ts(NOW.0 + 400 * DAY_MS))
            .unwrap()
            .is_empty()
    );
    store.close().unwrap();
}

#[test]
fn retention_drops_node_samples_older_than_31_days() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    let bucket_now = NOW.0 / 1000 / BUCKET_SEC;
    let conn = fresh(&path);
    for (name, age_days) in [("alpha-00001", 30), ("beta-00002", 32)] {
        conn.execute(
            "INSERT INTO node_samples(node, bucket, up, total, clients) VALUES (?1, ?2, 1, 1, 0)",
            rusqlite::params![name, bucket_now - age_days * 96],
        )
        .unwrap();
    }
    drop(conn);

    store.retain(NOW).unwrap();

    assert_eq!(count(&path, "SELECT count(*) FROM node_samples"), 1);
    assert_eq!(
        count(
            &path,
            "SELECT count(*) FROM node_samples WHERE node = 'alpha-00001'"
        ),
        1
    );
    store.close().unwrap();
}

#[test]
fn client_samples_are_written_and_replaced_by_their_key() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    let rows = |total: u64| {
        vec![
            (String::new(), total),
            ("a.example.com".to_owned(), total / 2),
        ]
    };
    store.send(HistoryOp::ClientSamples {
        at: NOW,
        rows: rows(10),
    });
    // The same moment again replaces, another moment adds.
    store.send(HistoryOp::ClientSamples {
        at: NOW,
        rows: rows(20),
    });
    store.send(HistoryOp::ClientSamples {
        at: Ts(NOW.0 + 30_000),
        rows: rows(30),
    });
    store.flush().unwrap();
    assert_eq!(count(&path, "SELECT count(*) FROM client_samples"), 4);
    assert_eq!(
        count(
            &path,
            &format!(
                "SELECT clients FROM client_samples WHERE at = {} AND domain = ''",
                NOW.0
            )
        ),
        20
    );
}

#[test]
fn samples_older_than_8_days_are_deleted() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    for ms in [8 * DAY_MS + 1000, 8 * DAY_MS - 1000, 0] {
        store.send(HistoryOp::ClientSamples {
            at: Ts(NOW.0 - ms),
            rows: vec![(String::new(), 1), ("a.example.com".to_owned(), 1)],
        });
    }
    store.flush().unwrap();
    assert_eq!(count(&path, "SELECT count(*) FROM client_samples"), 6);

    store.retain(NOW).unwrap();

    assert_eq!(count(&path, "SELECT count(*) FROM client_samples"), 4);
    assert_eq!(
        count(
            &path,
            &format!(
                "SELECT count(*) FROM client_samples WHERE at < {}",
                NOW.0 - 8 * DAY_MS
            )
        ),
        0
    );
}

#[test]
fn unknown_schema_version_refuses_open() {
    let (_dir, path) = temp_db();
    Store::open(&path).unwrap().close().unwrap();
    fresh(&path)
        .execute(
            "INSERT INTO schema_migrations(version, applied_at) VALUES (999, 0)",
            [],
        )
        .unwrap();

    let err = Store::open(&path).err().expect("must refuse to open");
    assert!(
        matches!(
            err,
            StoreError::UnknownSchemaVersion {
                found: 999,
                supported: 10
            }
        ),
        "{err:?}"
    );
    let text = err.to_string();
    assert!(text.contains("999") && text.contains("newer"), "{text}");
}

#[test]
fn corrupt_file_refuses_open() {
    let (_dir, path) = temp_db();
    std::fs::write(&path, b"this is not a database, ".repeat(400)).unwrap();

    let err = Store::open(&path).err().expect("must refuse to open");
    assert!(matches!(err, StoreError::Corrupt { .. }), "{err:?}");
    assert!(err.to_string().contains("corrupt"), "{err}");
}

#[test]
fn critical_ops_written_with_full_sync() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    let settings = Settings {
        proxy_port: 8443,
        ..Settings::default()
    };

    store.send(PersistOp::SetSettings(settings.clone()));
    store.send(event(EventKind::NodeRegistered, NOW));
    store.flush().unwrap();

    // The writer used PRAGMA synchronous=FULL (2) for the immediate ops.
    assert_eq!(store.last_immediate_sync(), 2);
    // The rows are visible from a fresh connection right after flush.
    let conn = fresh(&path);
    let json: String = conn
        .query_row("SELECT json FROM settings WHERE id = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(serde_json::from_str::<Settings>(&json).unwrap(), settings);
    let kind: String = conn
        .query_row("SELECT kind FROM events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(kind, "node_registered");
}

#[test]
fn batched_ops_flush_every_15s_and_on_shutdown() {
    assert_eq!(
        StoreOptions::default().batch_interval,
        Duration::from_secs(15)
    );

    // The timer flushes the batch without a flush call.
    let (_dir, path) = temp_db();
    let store = Store::open_with(
        &path,
        StoreOptions {
            batch_interval: Duration::from_millis(100),
        },
    )
    .unwrap();
    store.send(traffic(NOW));
    wait_until("the timer to flush the batch", || {
        count(&path, "SELECT count(*) FROM traffic") == 1
    });
    store.close().unwrap();

    // With the default interval a batched op waits, an immediate one does not, and close writes
    // what is queued.
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    store.send(traffic(NOW));
    store.send(event(EventKind::NodeRegistered, NOW));
    wait_until("the immediate op", || {
        count(&path, "SELECT count(*) FROM events") == 1
    });
    assert_eq!(count(&path, "SELECT count(*) FROM traffic"), 0);
    store.close().unwrap();
    assert_eq!(count(&path, "SELECT count(*) FROM traffic"), 1);
}

#[test]
fn write_error_sets_unhealthy() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    assert_eq!(store.health(), WriteHealth::Ok);

    fresh(&path).execute("DROP TABLE assignments", []).unwrap();
    store.send(PersistOp::SetMasters {
        domain: "pool.example.com".into(),
        slots: vec![],
    });
    // The writer goes on after the failure.
    store.send(PersistOp::SetSettings(Settings::default()));
    let err = store.flush().expect_err("flush reports the failed write");
    assert!(
        matches!(err, StoreError::WriteFailed { failed: 1, .. }),
        "{err:?}"
    );

    match store.health() {
        WriteHealth::Failed { error, at } => {
            assert!(error.contains("assignments"), "{error}");
            assert!(at.0 > 0);
        }
        WriteHealth::Ok => panic!("a failed write must make the store unhealthy"),
    }
    assert_eq!(
        count(
            &path,
            "SELECT count(*) FROM events WHERE kind = 'store_error'"
        ),
        1
    );
    assert_eq!(count(&path, "SELECT count(*) FROM settings"), 1);
    // The failure is not forgotten by a later good write: memory and disk diverged.
    assert!(matches!(store.health(), WriteHealth::Failed { .. }));
}

fn full_state() -> (PoolState, Vec<PersistOp>) {
    let settings = Settings {
        proxy_port: 8443,
        dns_ttl: 120,
        tls_domain: "tls.example.com".into(),
        domains: vec!["pool.example.com".into(), "pool1.example.com".into()],
        ..Settings::default()
    };
    settings.validate().expect("the test settings are valid");

    let mut healthy = node("alpha-a1b2c", "198.51.100.1", NodeState::Healthy);
    healthy.health.tcp.up = true;
    healthy.health.metrics.up = true;
    healthy.health.gp = GpStatus::Ok {
        at: Ts(NOW.0 - 1000),
    };
    let mut quarantine = node("bravo-d3e4f", "198.51.100.2", NodeState::Quarantine);
    quarantine.suspected = true;
    quarantine.undecided_rounds = 2;
    quarantine.last_round_at = Some(Ts(NOW.0 - 5000));
    let mut banned = node("charlie-g5h6i", "198.51.100.3", NodeState::Banned);
    banned.reverify = true;

    // A later health op replaces the health inside the record.
    let fresher = NodeHealth {
        last_heartbeat: Ts(NOW.0 - 10),
        ..healthy.health
    };
    let mut healthy_after = healthy.clone();
    healthy_after.health = fresher;

    let mut state = PoolState::new(settings.clone());
    for n in [&healthy_after, &quarantine, &banned] {
        state.nodes.insert(n.id.clone(), n.clone());
    }
    state.stats = BTreeMap::from([
        (
            healthy.id.clone(),
            NodeStats {
                counters: Some(TrafficCounters {
                    from_client: 100,
                    to_client: 200,
                }),
                from_client_total: 100,
                to_client_total: 200,
                clients: Some(4),
                other_counters: Some(TrafficCounters {
                    from_client: 300,
                    to_client: 400,
                }),
                other_from_client_total: 30,
                other_to_client_total: 40,
                other_clients: Some(5),
                port_filter: None,
                availability_bp: None,
            },
        ),
        (quarantine.id.clone(), NodeStats::default()),
        (banned.id.clone(), NodeStats::default()),
    ]);
    state.blocks = vec![IpBlock {
        ip: banned.ip,
        node_id: banned.id.clone(),
        blocked_at: Ts(NOW.0 - 2 * DAY_MS),
        next_reverify_at: Some(Ts(NOW.0 + 1000)),
        owner: Some(OwnerId(1)),
    }];
    state.tombstones = vec![
        Tombstone {
            node_id: id("delta-j7k8l"),
            ip: ip("198.51.100.4"),
            strikes: 2,
            until: Ts(NOW.0 + 500),
            updated_at: Ts(NOW.0 - 3 * DAY_MS),
        },
        Tombstone {
            node_id: id("echo-m9n0o"),
            ip: ip("198.51.100.5"),
            strikes: 1,
            until: Ts(NOW.0 - 100),
            updated_at: Ts(NOW.0 - DAY_MS),
        },
    ];
    state.assignments = Assignments(BTreeMap::from([(
        "pool.example.com".to_string(),
        vec![Slot {
            node: healthy.id.clone(),
            since: Ts(NOW.0 - 777),
        }],
    )]));
    state.dns = BTreeMap::from([
        (
            "pool.example.com".to_string(),
            DnsState {
                desired: RecordValue::A(vec![healthy.ip]),
                applied: Some(RecordValue::A(vec![ip("198.51.100.9")])),
                generation: Generation(5),
                retries: 1,
                next_try: Ts(NOW.0 + 60_000),
                in_flight: None,
            },
        ),
        (
            "pool1.example.com".to_string(),
            DnsState {
                desired: RecordValue::Cname("pool.example.com".into()),
                applied: None,
                generation: Generation(1),
                retries: 0,
                next_try: Ts(0),
                in_flight: None,
            },
        ),
    ]);
    state.srmd = SrmdState {
        created: ["pool1.example.com".to_string()].into(),
        cnames: BTreeMap::from([(
            "pool1.example.com".to_string(),
            "pool.example.com".to_string(),
        )]),
        ..SrmdState::default()
    };

    let mut ops = vec![
        PersistOp::SetSettings(settings),
        PersistOp::UpsertNode(healthy.clone()),
        PersistOp::UpsertNode(quarantine.clone()),
        PersistOp::UpsertNode(banned.clone()),
        PersistOp::NodeHealth {
            node: healthy.id.clone(),
            health: fresher,
        },
        // A node that is gone again.
        PersistOp::UpsertNode(node("gone-p1q2r", "198.51.100.6", NodeState::Pending)),
        PersistOp::DeleteNode(id("gone-p1q2r")),
        PersistOp::UpsertIpBlock(state.blocks[0].clone()),
        PersistOp::UpsertIpBlock(IpBlock {
            ip: ip("198.51.100.7"),
            node_id: id("gone-p1q2r"),
            blocked_at: Ts(NOW.0),
            next_reverify_at: None,
            owner: None,
        }),
        PersistOp::DeleteIpBlock {
            ip: ip("198.51.100.7"),
        },
        PersistOp::UpsertTombstone(state.tombstones[0].clone()),
        PersistOp::UpsertTombstone(state.tombstones[1].clone()),
        PersistOp::UpsertTombstone(Tombstone {
            node_id: id("gone-p1q2r"),
            ip: ip("198.51.100.6"),
            strikes: 1,
            until: Ts(NOW.0),
            updated_at: Ts(NOW.0),
        }),
        PersistOp::DeleteTombstone {
            node_id: id("gone-p1q2r"),
            ip: ip("198.51.100.6"),
        },
        PersistOp::SetMasters {
            domain: "stale.example.com".into(),
            slots: vec![],
        },
        PersistOp::DeleteMasters {
            domain: "stale.example.com".into(),
        },
        PersistOp::SetMasters {
            domain: "pool.example.com".into(),
            slots: state.assignments.0["pool.example.com"].clone(),
        },
        PersistOp::SetSrmd(SrmdState {
            // Counters and client counts are not stored.
            domain_clients: BTreeMap::from([("pool.example.com".to_string(), 9)]),
            expand_ticks: 3,
            fold_ticks: 4,
            ..state.srmd.clone()
        }),
    ];
    for (domain, dns) in &state.dns {
        ops.push(PersistOp::UpsertDns {
            domain: domain.clone(),
            dns: dns.clone(),
        });
    }
    for (node, stats) in &state.stats {
        ops.push(PersistOp::NodeStats {
            node: node.clone(),
            stats: stats.clone(),
        });
    }
    (state, ops)
}

#[test]
fn state_roundtrip_equals() {
    let (_dir, path) = temp_db();
    let (expected, ops) = full_state();
    let store = Store::open(&path).unwrap();
    for op in ops {
        store.send(op);
    }
    store.flush().unwrap();
    assert_eq!(store.health(), WriteHealth::Ok);

    assert_eq!(store.load_at(NOW).unwrap(), expected);
    // A restart reads the same state back.
    store.close().unwrap();
    let store = Store::open(&path).unwrap();
    assert_eq!(store.load_at(NOW).unwrap(), expected);
}

#[test]
fn load_drops_dns_rows_of_unknown_domains() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    let dns = |last: u8| DnsState {
        desired: RecordValue::A(vec![Ipv4Addr::new(198, 51, 100, last)]),
        applied: None,
        generation: Generation(1),
        retries: 0,
        next_try: Ts(0),
        in_flight: None,
    };
    store.send(PersistOp::SetSettings(Settings {
        domains: vec!["pool.example.com".into()],
        ..Settings::default()
    }));
    store.send(PersistOp::UpsertDns {
        domain: "pool.example.com".into(),
        dns: dns(1),
    });
    store.send(PersistOp::UpsertDns {
        domain: "removed.example.com".into(),
        dns: dns(2),
    });
    store.flush().unwrap();

    let state = store.load_at(NOW).unwrap();
    assert_eq!(state.dns.keys().collect::<Vec<_>>(), ["pool.example.com"]);
}

#[test]
fn load_gives_a_node_without_stats_default_stats() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    // The batched stats of a fresh node may be lost in a crash.
    store.send(PersistOp::UpsertNode(node(
        "alpha-a1b2c",
        "198.51.100.1",
        NodeState::Healthy,
    )));
    store.flush().unwrap();

    let state = store.load_at(NOW).unwrap();
    assert_eq!(state.stats[&id("alpha-a1b2c")], NodeStats::default());
}

#[test]
fn upsert_node_supersedes_queued_health() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    let mut n = node("alpha-a1b2c", "198.51.100.1", NodeState::Healthy);
    store.send(PersistOp::UpsertNode(n.clone()));
    // Older health, batched; the newer record below is written at once and must win.
    store.send(PersistOp::NodeHealth {
        node: n.id.clone(),
        health: n.health,
    });
    n.health.last_heartbeat = Ts(NOW.0);
    store.send(PersistOp::UpsertNode(n.clone()));
    store.flush().unwrap();

    assert_eq!(store.load_at(NOW).unwrap().nodes[&n.id], n);
}

#[test]
fn delete_node_drops_its_queued_batched_writes() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    let n = node("alpha-a1b2c", "198.51.100.1", NodeState::Healthy);
    store.send(PersistOp::UpsertNode(n.clone()));
    store.send(PersistOp::NodeStats {
        node: n.id.clone(),
        stats: NodeStats {
            clients: Some(1),
            ..NodeStats::default()
        },
    });
    store.send(PersistOp::DeleteNode(n.id.clone()));
    store.flush().unwrap();

    assert_eq!(count(&path, "SELECT count(*) FROM nodes"), 0);
    assert_eq!(count(&path, "SELECT count(*) FROM node_stats"), 0);
}

#[test]
fn round_is_stored_with_every_vote() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    let blocked = SensorVote {
        vote: Vote::Blocked,
        evidence: json!({ "probes": 12 }),
    };
    let abstain = SensorVote {
        vote: Vote::Abstain(AbstainReason::Timeout),
        evidence: json!(null),
    };
    store.send(PersistOp::RecordRound {
        node: id("alpha-a1b2c"),
        generation: Generation(3),
        at: NOW,
        verdict: RoundVerdict::Undecided,
        votes: vec![
            (SensorKind::Globalping, blocked.clone()),
            (SensorKind::Cheburcheck, abstain.clone()),
        ],
    });
    store.flush().unwrap();

    let conn = fresh(&path);
    let (node_id, generation, at, verdict): (String, i64, i64, String) = conn
        .query_row(
            "SELECT node, generation, at, verdict FROM sensor_rounds",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        (node_id.as_str(), generation, at, verdict.as_str()),
        ("alpha-a1b2c", 3, NOW.0, "undecided")
    );
    let mut stmt = conn
        .prepare("SELECT sensor, vote, data FROM sensor_votes ORDER BY sensor")
        .unwrap();
    let rows: Vec<(String, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        (rows[0].0.as_str(), rows[0].1.as_str()),
        ("cheburcheck", "abstain")
    );
    assert_eq!(
        serde_json::from_str::<SensorVote>(&rows[0].2).unwrap(),
        abstain
    );
    assert_eq!(
        (rows[1].0.as_str(), rows[1].1.as_str()),
        ("globalping", "blocked")
    );
    assert_eq!(
        serde_json::from_str::<SensorVote>(&rows[1].2).unwrap(),
        blocked
    );
}

#[test]
fn lift_ban_closes_the_open_rows_of_that_ip() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    let ban = |addr: &str, at: i64, reverify: bool| PersistOp::RecordBan {
        ip: ip(addr),
        node: id("alpha-a1b2c"),
        at: Ts(at),
        votes: vec![
            (SensorKind::Globalping, Vote::Blocked),
            (SensorKind::RedFlag, Vote::Blocked),
        ],
        reverify,
        node_registered_at: None,
    };
    store.send(ban("198.51.100.1", 100, false));
    store.send(ban("198.51.100.1", 200, true));
    store.send(ban("198.51.100.2", 300, false));
    store.send(PersistOp::LiftBan {
        ip: ip("198.51.100.1"),
        at: Ts(400),
        admin: true,
    });
    store.flush().unwrap();

    let conn = fresh(&path);
    let mut stmt = conn
        .prepare("SELECT ip, at, reverify, lifted_at, lifted_by_admin FROM bans ORDER BY id")
        .unwrap();
    let rows: Vec<(String, i64, i64, Option<i64>, i64)> = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        rows,
        vec![
            ("198.51.100.1".into(), 100, 0, Some(400), 1),
            ("198.51.100.1".into(), 200, 1, Some(400), 1),
            ("198.51.100.2".into(), 300, 0, None, 0),
        ]
    );
    let votes: String = conn
        .query_row("SELECT votes FROM bans WHERE at = 100", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Vec<(SensorKind, Vote)>>(&votes).unwrap(),
        vec![
            (SensorKind::Globalping, Vote::Blocked),
            (SensorKind::RedFlag, Vote::Blocked)
        ]
    );
}

#[test]
fn retention_removes_old_events_30d_and_keeps_traffic() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    let at = |days: i64, extra_ms: i64| Ts(NOW.0 - days * DAY_MS + extra_ms);

    // Events: 29d23h and 30d1h old.
    store.send(event(EventKind::NodeRegistered, at(30, 3_600_000)));
    store.send(event(EventKind::NodeBanned, at(30, -3_600_000)));
    // Traffic is the total of the pool: a year-old hour stays.
    store.send(traffic(at(365, 0)));
    store.send(traffic(at(31, 0)));
    store.flush().unwrap();
    assert_eq!(count(&path, "SELECT count(*) FROM events"), 2);
    assert_eq!(count(&path, "SELECT count(*) FROM traffic"), 2);

    store.retain(NOW).unwrap();

    assert_eq!(count(&path, "SELECT count(*) FROM events"), 1);
    assert_eq!(
        count(
            &path,
            "SELECT count(*) FROM events WHERE kind = 'node_registered'"
        ),
        1
    );
    assert_eq!(count(&path, "SELECT count(*) FROM traffic"), 2);
}

#[test]
fn tombstones_removed_after_7d() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    let tombstone = |name: &str, last: u8, age_ms: i64| Tombstone {
        node_id: id(name),
        ip: Ipv4Addr::new(198, 51, 100, last),
        strikes: 1,
        until: Ts(NOW.0),
        updated_at: Ts(NOW.0 - age_ms),
    };
    let fresh_one = tombstone("alpha-a1b2c", 1, 7 * DAY_MS - 1000);
    store.send(PersistOp::UpsertTombstone(fresh_one.clone()));
    store.send(PersistOp::UpsertTombstone(tombstone(
        "bravo-d3e4f",
        2,
        7 * DAY_MS,
    )));
    store.send(PersistOp::UpsertTombstone(tombstone(
        "charlie-g5h6i",
        3,
        9 * DAY_MS,
    )));
    store.flush().unwrap();

    // Load skips the expired ones even before retention ran.
    assert_eq!(
        store.load_at(NOW).unwrap().tombstones,
        vec![fresh_one.clone()]
    );
    assert_eq!(count(&path, "SELECT count(*) FROM tombstones"), 3);

    store.retain(NOW).unwrap();

    assert_eq!(count(&path, "SELECT count(*) FROM tombstones"), 1);
    assert_eq!(store.load_at(NOW).unwrap().tombstones, vec![fresh_one]);
}

#[test]
fn invalid_stored_settings_stop_load() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    let broken = Settings {
        heartbeat_ttl_sec: 0,
        ..Settings::default()
    };
    store.send(PersistOp::SetSettings(broken));
    store.flush().unwrap();

    let err = store
        .load_at(NOW)
        .expect_err("invalid settings must not load");
    assert!(matches!(err, StoreError::InvalidData(_)), "{err:?}");
    assert!(err.to_string().contains("settings"), "{err}");
}

#[test]
fn close_is_idempotent_and_later_sends_mark_the_store_unhealthy() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    store.close().unwrap();
    store.close().unwrap();

    store.send(PersistOp::SetSettings(Settings::default()));
    assert!(matches!(store.health(), WriteHealth::Failed { .. }));
    assert!(matches!(store.flush(), Err(StoreError::WriterStopped)));
}

#[test]
fn read_pool_hands_out_four_connections_and_takes_them_back() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    let held: Vec<ReadConn> = (0..4).map(|_| store.read()).collect();
    let (tx, rx) = std::sync::mpsc::channel();
    let s2 = store.clone();
    let waiter = std::thread::spawn(move || {
        let c = s2.read();
        tx.send(()).unwrap();
        drop(c);
    });
    assert!(
        rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "the fifth reader must wait"
    );
    drop(held);
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    waiter.join().unwrap();
}

#[test]
fn read_connections_cannot_write() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    let conn = store.read();
    assert!(conn.execute("DELETE FROM settings", []).is_err());
}

#[test]
fn retention_removes_sensor_rounds_35d() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    let vote = SensorVote {
        vote: Vote::Clear,
        evidence: json!(null),
    };
    let round = |days: i64| PersistOp::RecordRound {
        node: id("alpha-a1b2c"),
        generation: Generation(1),
        at: Ts(NOW.0 - days * DAY_MS),
        verdict: RoundVerdict::Undecided,
        votes: vec![
            (SensorKind::Globalping, vote.clone()),
            (SensorKind::Cheburcheck, vote.clone()),
        ],
    };
    store.send(round(36));
    store.send(round(34));
    store.flush().unwrap();
    assert_eq!(count(&path, "SELECT count(*) FROM sensor_rounds"), 2);
    assert_eq!(count(&path, "SELECT count(*) FROM sensor_votes"), 4);

    store.retain(NOW).unwrap();

    assert_eq!(count(&path, "SELECT count(*) FROM sensor_rounds"), 1);
    assert_eq!(
        count(
            &path,
            &format!(
                "SELECT count(*) FROM sensor_rounds WHERE at = {}",
                NOW.0 - 34 * DAY_MS
            )
        ),
        1
    );
    // The cascade removed the votes of the old round and only those.
    assert_eq!(count(&path, "SELECT count(*) FROM sensor_votes"), 2);
    assert_eq!(
        count(
            &path,
            "SELECT count(*) FROM sensor_votes \
             WHERE round_id NOT IN (SELECT id FROM sensor_rounds)"
        ),
        0
    );
}

fn token_row(kind: &'static str, seed: u8) -> NewTokenRow {
    NewTokenRow {
        kind,
        hash: [seed; 32],
        prefix: "mtp_x".into(),
        owner_id: None,
        node_id: None,
        expires_at: None,
        scope: String::new(),
        created_at: NOW,
    }
}

#[test]
fn auth_ops_written_with_full_sync_and_readable_after_flush() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    store.send(AuthOp::InsertSession {
        hash: [1; 32],
        csrf: "c".into(),
        created_at: NOW,
        expires_at: Ts(NOW.0 + 1000),
    });
    store.flush().unwrap();
    assert_eq!(store.last_immediate_sync(), 2);
    let n: i64 = store
        .read()
        .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);

    // An insert clears the sessions that have ended.
    store.send(AuthOp::InsertSession {
        hash: [2; 32],
        csrf: "d".into(),
        created_at: Ts(NOW.0 + 2000),
        expires_at: Ts(NOW.0 + 5000),
    });
    store.flush().unwrap();
    assert_eq!(count(&path, "SELECT count(*) FROM sessions"), 1);
    store.send(AuthOp::DeleteSession { hash: [2; 32] });
    store.flush().unwrap();
    assert_eq!(count(&path, "SELECT count(*) FROM sessions"), 0);
}

#[test]
fn rotate_admin_replaces_tokens_and_sessions_in_one_step() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    store.send(AuthOp::InsertToken(token_row("admin", 1)));
    store.send(AuthOp::InsertToken(token_row("metrics", 2)));
    store.send(AuthOp::InsertSession {
        hash: [9; 32],
        csrf: "c".into(),
        created_at: NOW,
        expires_at: Ts(NOW.0 + 1000),
    });
    store.send(AuthOp::RotateAdmin(token_row("admin", 3)));
    store.flush().unwrap();

    assert_eq!(count(&path, "SELECT count(*) FROM sessions"), 0);
    assert_eq!(
        count(
            &path,
            "SELECT count(*) FROM tokens WHERE kind = 'admin' AND revoked_at IS NULL"
        ),
        1
    );
    assert_eq!(
        count(
            &path,
            "SELECT count(*) FROM tokens WHERE kind = 'metrics' AND revoked_at IS NULL"
        ),
        1
    );
    assert_eq!(
        count(
            &path,
            "SELECT count(*) FROM tokens WHERE hash = x'0303030303030303030303030303030303030303030303030303030303030303' AND revoked_at IS NULL"
        ),
        1
    );
}

#[test]
fn auth_op_debug_does_not_print_secrets() {
    let op = AuthOp::InsertSession {
        hash: [7; 32],
        csrf: "very-secret-csrf".into(),
        created_at: NOW,
        expires_at: NOW,
    };
    assert_eq!(format!("{op:?}"), "insert_session");
}

#[test]
fn recorded_measurements_are_seen_and_old_ones_forgotten() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    let record = |m: &str, at: i64| {
        store.send(ReportOp::RecordMeasurement {
            id: m.into(),
            node: id("alpha-00001"),
            at: Ts(at),
        });
        store.flush().unwrap();
    };
    record("old", NOW.0 - DAY_MS - 1);
    record("fresh", NOW.0);
    // Recording the same id again changes nothing and does not fail the writer.
    record("fresh", NOW.0 + 5);
    assert_eq!(store.health(), WriteHealth::Ok);

    let conn = store.read();
    assert!(measurement_seen(&conn, "fresh").unwrap());
    assert!(
        !measurement_seen(&conn, "old").unwrap(),
        "forgotten after a day"
    );
    assert!(!measurement_seen(&conn, "other").unwrap());
    drop(conn);
    assert_eq!(count(&path, "SELECT count(*) FROM gp_measurements"), 1);
    assert_eq!(
        count(&path, "SELECT at FROM gp_measurements WHERE id = 'fresh'"),
        NOW.0
    );
}

/// A fresh install starts the service and runs an admin command at the same moment, and both
/// open the new database.
#[test]
fn processes_opening_a_new_database_together_all_succeed() {
    for _ in 0..20 {
        let (_dir, path) = temp_db();
        let start = std::sync::Barrier::new(6);
        let results: Vec<Result<(), String>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..6)
                .map(|_| {
                    scope.spawn(|| {
                        start.wait();
                        let store = Store::open(&path).map_err(|e| e.to_string())?;
                        store.close().map_err(|e| e.to_string())
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for result in results {
            assert_eq!(result, Ok(()));
        }
        assert_eq!(count(&path, "SELECT count(*) FROM schema_migrations"), 10);
    }
}

/// The database an older build left: the migrations up to `version`, nothing else.
fn database_at(path: &Path, version: i64) {
    let conn = Connection::open(path).unwrap();
    for (v, sql) in MIGRATIONS.iter().filter(|(v, _)| *v <= version) {
        conn.execute_batch(sql).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations(version, applied_at) VALUES (?1, 0)",
            [v],
        )
        .unwrap();
    }
}

fn version_of(path: &Path) -> i64 {
    count(path, "SELECT MAX(version) FROM schema_migrations")
}

fn a_ban_row(conn: &Connection, node: &str, reverify: i64) {
    conn.execute(
        "INSERT INTO bans(ip, node, at, votes, reverify) VALUES ('192.0.2.1', ?1, 9000, '[]', ?2)",
        rusqlite::params![node, reverify],
    )
    .unwrap();
}

#[test]
fn the_migration_keeps_when_the_nodes_of_old_bans_registered() {
    let (_dir, path) = temp_db();
    database_at(&path, 7);
    let conn = fresh(&path);
    conn.execute(
        "INSERT INTO nodes(id, owner, ip, port, engine, state, record) \
         VALUES ('old-00001', 1, '192.0.2.1', 443, 'telemt', 'banned', '{\"registered_at\": 1000}')",
        [],
    )
    .unwrap();
    a_ban_row(&conn, "old-00001", 0);
    a_ban_row(&conn, "old-00001", 1);
    a_ban_row(&conn, "gone-00002", 0);
    drop(conn);

    Store::open(&path).unwrap().close().unwrap();

    let conn = fresh(&path);
    let mut stmt = conn
        .prepare("SELECT node_registered_at FROM bans ORDER BY id")
        .unwrap();
    let kept: Vec<Option<i64>> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(kept, vec![Some(1000), None, None]);
}

#[test]
fn the_migration_finds_old_bans_in_the_journal_of_registrations() {
    let (_dir, path) = temp_db();
    database_at(&path, 8);
    let conn = fresh(&path);
    let registered = |node: &str, at: i64| {
        conn.execute(
            "INSERT INTO events(at, kind, node, detail) VALUES (?1, 'node_registered', ?2, '{}')",
            rusqlite::params![at, node],
        )
        .unwrap();
    };
    // Banned at 9000, as every ban row here: it registered twice before that and once after.
    registered("twice-00001", 1000);
    registered("twice-00001", 5000);
    registered("twice-00001", 9500);
    registered("once-00002", 2000);
    registered("kept-00003", 3000);
    registered("recheck-00004", 4000);
    conn.execute(
        "INSERT INTO events(at, kind, node, detail) VALUES (6000, 'node_retired', 'late-00005', '{}')",
        [],
    )
    .unwrap();
    a_ban_row(&conn, "twice-00001", 0);
    a_ban_row(&conn, "once-00002", 0);
    a_ban_row(&conn, "kept-00003", 0);
    conn.execute(
        "UPDATE bans SET node_registered_at = 7777 WHERE node = 'kept-00003'",
        [],
    )
    .unwrap();
    a_ban_row(&conn, "recheck-00004", 1);
    a_ban_row(&conn, "late-00005", 0);
    a_ban_row(&conn, "gone-00006", 0);
    drop(conn);

    Store::open(&path).unwrap().close().unwrap();

    let conn = fresh(&path);
    let mut stmt = conn
        .prepare("SELECT node_registered_at FROM bans ORDER BY id")
        .unwrap();
    let kept: Vec<Option<i64>> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        kept,
        vec![Some(5000), Some(2000), Some(7777), None, None, None]
    );
}

const HOUR_MS: i64 = 3_600_000;

#[test]
fn the_migration_merges_stored_traffic_by_hour_and_keeps_it_the_pools() {
    let (_dir, path) = temp_db();
    database_at(&path, 8);
    let conn = fresh(&path);
    let row = |node: &str, at: i64, from: i64, to: i64| {
        conn.execute(
            "INSERT INTO traffic(node, at, from_client, to_client) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![node, at, from, to],
        )
        .unwrap();
    };
    row("old-00001", 10 * HOUR_MS + 1, 10, 20);
    row("old-00001", 10 * HOUR_MS + 2, 1, 2);
    row("old-00001", 11 * HOUR_MS, 100, 200);
    row("new-00002", 10 * HOUR_MS + 3, 5, 6);
    drop(conn);

    Store::open(&path).unwrap().close().unwrap();

    assert_eq!(count(&path, "SELECT count(*) FROM traffic"), 3);
    assert_eq!(
        count(
            &path,
            &format!(
                "SELECT count(*) FROM traffic WHERE node = 'old-00001' AND at = {} \
                 AND from_client = 11 AND to_client = 22 \
                 AND other_from_client = 0 AND other_to_client = 0",
                10 * HOUR_MS
            )
        ),
        1
    );
}

#[test]
fn the_migration_keeps_what_the_nodes_counted_of_the_other_users() {
    let (_dir, path) = temp_db();
    database_at(&path, 8);
    let conn = fresh(&path);
    let stats = |node: &str, json: &str| {
        conn.execute(
            "INSERT INTO node_stats(node, stats) VALUES (?1, ?2)",
            rusqlite::params![node, json],
        )
        .unwrap();
    };
    stats(
        "own-00001",
        r#"{"other_from_client_total": 30, "other_to_client_total": 40}"#,
    );
    stats(
        "none-00002",
        r#"{"other_from_client_total": 0, "other_to_client_total": 0}"#,
    );
    stats(
        "older-00003",
        r#"{"from_client_total": 7, "to_client_total": 8}"#,
    );
    drop(conn);

    Store::open(&path).unwrap().close().unwrap();

    assert_eq!(count(&path, "SELECT count(*) FROM traffic"), 1);
    assert_eq!(
        count(
            &path,
            "SELECT count(*) FROM traffic WHERE node = 'own-00001' AND from_client = 0 \
             AND to_client = 0 AND other_from_client = 30 AND other_to_client = 40"
        ),
        1
    );
}

#[test]
fn the_migration_adds_the_other_users_to_a_node_that_already_has_traffic_this_hour() {
    let (_dir, path) = temp_db();
    database_at(&path, 8);
    let conn = fresh(&path);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    conn.execute(
        "INSERT INTO traffic(node, at, from_client, to_client) VALUES ('own-00001', ?1, 5, 6)",
        [now_ms],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO node_stats(node, stats) VALUES ('own-00001', ?1)",
        [r#"{"other_from_client_total": 30, "other_to_client_total": 40}"#],
    )
    .unwrap();
    drop(conn);

    Store::open(&path).unwrap().close().unwrap();

    assert_eq!(count(&path, "SELECT count(*) FROM traffic"), 1);
    assert_eq!(
        count(
            &path,
            "SELECT count(*) FROM traffic WHERE node = 'own-00001' AND from_client = 5 \
             AND to_client = 6 AND other_from_client = 30 AND other_to_client = 40"
        ),
        1
    );
}

#[test]
fn traffic_of_one_hour_is_one_row() {
    let (_dir, path) = temp_db();
    let store = Store::open(&path).unwrap();
    let hour = NOW.0 - NOW.0 % HOUR_MS;
    store.send(traffic(Ts(hour + 1)));
    store.send(traffic(Ts(hour + HOUR_MS - 1)));
    store.send(traffic(Ts(hour + HOUR_MS)));
    store.close().unwrap();

    assert_eq!(count(&path, "SELECT count(*) FROM traffic"), 2);
    assert_eq!(
        count(
            &path,
            &format!(
                "SELECT count(*) FROM traffic WHERE at = {hour} AND from_client = 20 \
                 AND to_client = 40 AND other_from_client = 60 AND other_to_client = 80"
            )
        ),
        1
    );
}

#[test]
fn a_migration_first_keeps_a_copy_of_the_database() {
    let (_dir, path) = temp_db();
    database_at(&path, 7);
    a_ban_row(&fresh(&path), "old-00001", 0);

    Store::open(&path).unwrap().close().unwrap();

    let copy = crate::backup::before_migration(&path, 7);
    assert!(copy.is_file(), "{}", copy.display());
    assert_eq!(version_of(&copy), 7);
    assert_eq!(count(&copy, "SELECT COUNT(*) FROM bans"), 1);
    assert_eq!(version_of(&path), 10);

    // A restore checks the sum beside the copy, so the sum must be there.
    crate::backup::restore(&path, &copy).unwrap();
    assert_eq!(version_of(&path), 7);
}

#[test]
fn a_database_with_nothing_to_migrate_gets_no_copy() {
    let (dir, path) = temp_db();
    Store::open(&path).unwrap().close().unwrap();
    Store::open(&path).unwrap().close().unwrap();
    let copies: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".before-v"))
        .collect();
    assert!(copies.is_empty(), "{copies:?}");
}

#[test]
fn only_the_newest_copy_stays() {
    let (_dir, path) = temp_db();
    database_at(&path, 7);
    let old = crate::backup::before_migration(&path, 6);
    std::fs::write(&old, b"old").unwrap();
    let old_sum = std::path::PathBuf::from(format!("{}.sha256", old.display()));
    std::fs::write(&old_sum, b"old").unwrap();

    Store::open(&path).unwrap().close().unwrap();

    assert!(!old.exists());
    assert!(!old_sum.exists());
    assert!(crate::backup::before_migration(&path, 7).is_file());
}

/// The build that was put back finds the database of the one that failed.
fn rolled_back(path: &Path) {
    database_at(path, 7);
    a_ban_row(&fresh(path), "before-00001", 0);
    Store::open(path).unwrap().close().unwrap();
    a_ban_row(&fresh(path), "after-00002", 0);
}

#[test]
fn a_build_put_back_takes_the_database_it_knows() {
    let (_dir, path) = temp_db();
    rolled_back(&path);

    assert!(undo_schema(&path, 7, UNDO_WINDOW).unwrap());

    assert_eq!(version_of(&path), 7);
    assert_eq!(count(&path, "SELECT COUNT(*) FROM bans"), 1);
    assert!(!crate::backup::before_migration(&path, 7).exists());
    // Once only.
    assert!(!undo_schema(&path, 7, UNDO_WINDOW).unwrap());
}

#[test]
fn an_old_copy_is_not_taken_back() {
    let (_dir, path) = temp_db();
    rolled_back(&path);

    assert!(!undo_schema(&path, 7, Duration::ZERO).unwrap());

    assert_eq!(version_of(&path), 10, "the database is left as it was");
    assert!(crate::backup::before_migration(&path, 7).is_file());
}

#[test]
fn a_database_this_build_knows_is_left_alone() {
    let (_dir, path) = temp_db();
    rolled_back(&path);
    assert!(!undo_schema(&path, 10, UNDO_WINDOW).unwrap());
    assert_eq!(count(&path, "SELECT COUNT(*) FROM bans"), 2);
}

#[test]
fn a_newer_database_without_a_copy_is_still_refused() {
    let (_dir, path) = temp_db();
    Store::open(&path).unwrap().close().unwrap();
    fresh(&path)
        .execute(
            "INSERT INTO schema_migrations(version, applied_at) VALUES (999, 0)",
            [],
        )
        .unwrap();
    assert!(!undo_newer_schema(&path).unwrap());
    assert!(matches!(
        Store::open(&path),
        Err(StoreError::UnknownSchemaVersion { found: 999, .. })
    ));
}

#[test]
fn a_damaged_copy_is_not_taken_back() {
    let (_dir, path) = temp_db();
    rolled_back(&path);
    std::fs::write(crate::backup::before_migration(&path, 7), b"damaged").unwrap();

    assert!(undo_schema(&path, 7, UNDO_WINDOW).is_err());
    assert_eq!(version_of(&path), 10, "the database is left as it was");
}

#[test]
fn a_corrupt_file_is_left_for_the_open_to_explain() {
    let (_dir, path) = temp_db();
    std::fs::write(&path, b"this is not a database, ".repeat(400)).unwrap();
    assert!(!undo_newer_schema(&path).unwrap());
    assert!(matches!(
        Store::open(&path),
        Err(StoreError::Corrupt { .. })
    ));
}
