//! The report of the node, built from the node's own checks.

mod common;

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use common::*;
use mtpool_agent::engine::EngineError;
use mtpool_agent::engine::EngineHealth;
use mtpool_agent::engine::http::HttpResponse;
use mtpool_agent::engine::http::testing::ScriptedHttp;
use mtpool_agent::gp::GpLoop;
use mtpool_agent::ip_detect::{IP_SOURCES, IpWatch};
use mtpool_agent::lifecycle::{ConfigSink, Health, ReportSource};
use mtpool_agent::report::{
    ChecksSetup, ConfigMemory, NodeChecks, Remembering, ReportInputs, build_report,
};
use mtpool_agent::state_files::{DiskUsage, Fs, MemFs};
use mtpool_protocol::agent::{Dc2Report, GpReport, HostReport, MetricsReport};
use serde_json::json;
use tokio::time::Instant;

fn inputs() -> ReportInputs {
    ReportInputs {
        ip: "192.0.2.1".parse().unwrap(),
        port: 8443,
        sni: "tls.example.test".into(),
        gp: GpReport::Measured {
            id: "2BSGWjCSdcIMKXZfj00021RC5".into(),
        },
        metrics: Ok(MetricsReport {
            ok: true,
            from_client_total: 5,
            to_client_total: 7,
            clients: Some(2),
            other_clients: None,
            other_from_client_total: None,
            other_to_client_total: None,
        }),
        dc2: Some(Dc2Report {
            ok: true,
            reason: None,
        }),
        host: Some(HostReport {
            load1: 0.5,
            mem_used_bytes: 1,
            mem_total_bytes: 2,
            disk_used_bytes: 3,
            disk_total_bytes: 4,
        }),
        health: EngineHealth {
            ok: true,
            reason: None,
        },
        registrar_takes_other_users: true,
        // 2026-11-03T12:00:00Z and 123 ms.
        now: SystemTime::UNIX_EPOCH + Duration::from_millis(1_793_707_200_123),
    }
}

#[test]
fn the_report_has_every_part() {
    let report = build_report(inputs());
    assert_eq!(
        serde_json::to_value(&report).unwrap(),
        json!({
            "ip": "192.0.2.1",
            "port": 8443,
            "sni": "tls.example.test",
            "gp": { "kind": "measured", "id": "2BSGWjCSdcIMKXZfj00021RC5" },
            "metrics": { "ok": true, "from_client_total": 5, "to_client_total": 7, "clients": 2 },
            "dc2": { "ok": true, "reason": null },
            "host": {
                "load1": 0.5, "mem_used_bytes": 1, "mem_total_bytes": 2,
                "disk_used_bytes": 3, "disk_total_bytes": 4
            },
            "checked_at": "2026-11-03T12:00:00Z",
            "error": null
        })
    );
}

/// What an agent that tells the users apart gets from the engine.
fn split_metrics() -> MetricsReport {
    MetricsReport {
        ok: true,
        from_client_total: 5,
        to_client_total: 7,
        clients: Some(2),
        other_clients: Some(3),
        other_from_client_total: Some(100),
        other_to_client_total: Some(200),
    }
}

#[test]
fn a_registrar_that_takes_the_split_gets_it() {
    let mut input = inputs();
    input.metrics = Ok(split_metrics());
    assert_eq!(build_report(input).metrics, split_metrics());
}

/// The report as every released registrar up to v0.0.2 parses it: it refuses a field it does not
/// name, and the node would go without a report at all.
#[derive(serde::Deserialize, Debug)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct MetricsOfAnOlderRegistrar {
    ok: bool,
    from_client_total: u64,
    to_client_total: u64,
    clients: Option<u64>,
}

#[test]
fn a_registrar_that_does_not_gets_all_users_summed_as_older_agents_sent_them() {
    let mut input = inputs();
    input.metrics = Ok(split_metrics());
    input.registrar_takes_other_users = false;
    let metrics = build_report(input).metrics;
    assert_eq!(
        metrics,
        MetricsReport {
            ok: true,
            from_client_total: 105,
            to_client_total: 207,
            clients: Some(5),
            other_clients: None,
            other_from_client_total: None,
            other_to_client_total: None,
        }
    );
    let wire = serde_json::to_value(&metrics).unwrap();
    serde_json::from_value::<MetricsOfAnOlderRegistrar>(wire).expect("an older registrar takes it");

    // The split alone would be refused: this is why it is not sent.
    let wire = serde_json::to_value(split_metrics()).unwrap();
    assert!(serde_json::from_value::<MetricsOfAnOlderRegistrar>(wire).is_err());
}

#[test]
fn an_engine_without_per_user_numbers_stays_without_them_for_an_older_registrar() {
    let mut input = inputs();
    input.metrics = Ok(MetricsReport {
        ok: true,
        from_client_total: 0,
        to_client_total: 0,
        clients: None,
        other_clients: None,
        other_from_client_total: None,
        other_to_client_total: None,
    });
    input.registrar_takes_other_users = false;
    let metrics = build_report(input).metrics;
    assert_eq!(metrics.clients, None);
    assert_eq!((metrics.from_client_total, metrics.to_client_total), (0, 0));
}

#[test]
fn an_engine_that_does_not_answer_is_ok_false() {
    let mut input = inputs();
    input.metrics = Err(EngineError::Http("no answer".into()));
    let report = build_report(input);
    assert_eq!(
        report.metrics,
        MetricsReport {
            ok: false,
            from_client_total: 0,
            to_client_total: 0,
            clients: None,
            other_clients: None,
            other_from_client_total: None,
            other_to_client_total: None,
        }
    );
}

#[test]
fn user_telemetry_disabled_is_the_error_of_the_report() {
    let mut input = inputs();
    input.health = EngineHealth {
        ok: true,
        reason: Some("user_telemetry_disabled".into()),
    };
    assert_eq!(
        build_report(input).error.as_deref(),
        Some("user_telemetry_disabled")
    );

    let mut input = inputs();
    input.health = EngineHealth {
        ok: false,
        reason: Some("not serving".into()),
    };
    assert_eq!(build_report(input).error, None);
}

struct Node {
    checks: NodeChecks<ScriptedHttp>,
    engine: Arc<FakeEngine>,
    ports: Arc<FakePorts>,
    http: Arc<ScriptedHttp>,
    memory: ConfigMemory,
    gp: GpLoop,
}

fn node(port_override: Option<u16>) -> Node {
    let engine = FakeEngine::new();
    let ports = FakePorts::new(true);
    let http = Arc::new(ScriptedHttp::new());
    for url in IP_SOURCES {
        http.always(
            url,
            Ok(HttpResponse {
                status: 200,
                body: "192.0.2.1".into(),
            }),
        );
    }
    let fs = Arc::new(MemFs::new());
    fs.write_private(
        std::path::Path::new("/proc/loadavg"),
        b"0.52 0.5 0.5 1/1 1\n",
    )
    .unwrap();
    fs.write_private(
        std::path::Path::new("/proc/meminfo"),
        b"MemTotal: 1000 kB\nMemAvailable: 400 kB\n",
    )
    .unwrap();
    fs.set_disk_usage(DiskUsage {
        total_bytes: 100,
        used_bytes: 30,
    });
    let memory = ConfigMemory::new();
    let gp = GpLoop::new("http://127.0.0.1:9", ports.clone()).unwrap();
    let setup = ChecksSetup {
        engine: engine.clone(),
        port: port_override,
        ports: ports.clone(),
        fs: fs.clone(),
        ip: IpWatch::new(http.clone(), fs.clone(), "/state".into()),
        gp: gp.clone(),
        memory: memory.clone(),
    };
    Node {
        checks: NodeChecks::new(setup)
            .with_clock(|| SystemTime::UNIX_EPOCH + Duration::from_secs(1_793_707_200)),
        engine,
        ports,
        http,
        memory,
        gp,
    }
}

#[tokio::test]
async fn nothing_is_reported_before_the_address_and_the_domain_are_known() {
    let n = node(None);
    assert!(n.checks.build().await.is_none(), "no address, no domain");

    n.checks.watch(Instant::now()).await;
    assert!(n.checks.build().await.is_none(), "no domain yet");

    n.memory.remember(&agent_config(30, 60));
    let report = n.checks.build().await.expect("everything is known now");
    assert_eq!(report.ip, "192.0.2.1".parse::<Ipv4Addr>().unwrap());
    assert_eq!(report.sni, "tls.example.test");
    assert_eq!(report.port, 443, "the engine's own port");
    assert_eq!(report.checked_at, "2026-11-03T12:00:00Z");
    assert_eq!(
        report.gp,
        GpReport::NoData {
            reason: "no_data".into()
        }
    );
    assert!(report.metrics.ok);
    assert!(report.dc2.is_some());
    let host = report.host.unwrap();
    assert_eq!(
        (host.mem_total_bytes, host.mem_used_bytes),
        (1000 * 1024, 600 * 1024)
    );
    assert_eq!((host.disk_total_bytes, host.disk_used_bytes), (100, 30));
    assert_eq!(report.error, None);
}

#[tokio::test]
async fn the_first_measurement_starts_as_soon_as_the_domain_is_known() {
    let n = node(None);
    // The order of a fresh node: the checks, then the config, then the first report.
    n.checks.watch(Instant::now()).await;
    n.memory.remember(&agent_config(30, 60));
    n.checks.build().await.expect("everything is known now");

    n.gp.tick(Instant::now()).await;
    assert!(
        n.gp.take_result().is_some(),
        "the measurement does not wait for the next round of checks"
    );
}

#[tokio::test]
async fn the_local_port_wins_over_the_engines() {
    let n = node(Some(8443));
    n.checks.watch(Instant::now()).await;
    n.memory.remember(&agent_config(30, 60));
    assert_eq!(n.checks.build().await.unwrap().port, 8443);
    assert!(
        n.ports
            .asked
            .lock()
            .unwrap()
            .iter()
            .all(|(port, _)| *port == 8443)
    );
}

#[tokio::test]
async fn the_report_follows_what_the_last_config_said_about_the_registrar() {
    let n = node(None);
    n.checks.watch(Instant::now()).await;
    *n.engine.metrics.lock().unwrap() = Some(split_metrics());

    n.memory.remember(&agent_config(30, 60));
    assert_eq!(n.checks.build().await.unwrap().metrics, split_metrics());

    // The registrar was rolled back: the next config does not say it any more.
    let mut config = agent_config(30, 60);
    config.report_other_users = false;
    n.memory.remember(&config);
    let metrics = n.checks.build().await.unwrap().metrics;
    assert_eq!(metrics.from_client_total, 105);
    assert_eq!(metrics.other_from_client_total, None);
}

#[tokio::test]
async fn the_sink_remembers_the_config_and_hands_it_on() {
    let memory = ConfigMemory::new();
    let inner = RecordingSink::default();
    let sink = Remembering {
        memory: memory.clone(),
        inner: inner.clone(),
    };
    assert_eq!(memory.get(), None);
    sink.apply(&agent_config(30, 60)).await;
    assert_eq!(memory.get(), Some(("tls.example.test".to_owned(), 300)));
    assert_eq!(inner.applied().len(), 1);
}

#[tokio::test]
async fn watch_finds_a_changed_address_and_a_sick_proxy() {
    let n = node(None);
    let t0 = Instant::now();

    let first = n.checks.watch(t0).await;
    assert!(!first.ip_changed, "the first address is not a change");
    assert_eq!(first.health, Health::Good);
    assert_eq!(n.http.requests().len(), 4);

    // The proxy stops answering: three checks make it silent.
    n.ports.set(false);
    for i in 1..=2 {
        let watch = n.checks.watch(t0 + Duration::from_secs(30 * i)).await;
        assert_eq!(watch.health, Health::Good);
    }
    assert_eq!(
        n.checks.watch(t0 + Duration::from_secs(90)).await.health,
        Health::Silent
    );

    // The address changes.
    for url in IP_SOURCES {
        n.http.always(
            url,
            Ok(HttpResponse {
                status: 200,
                body: "198.51.100.4".into(),
            }),
        );
    }
    let changed = n.checks.watch(t0 + Duration::from_secs(135)).await;
    assert!(changed.ip_changed);
    assert!(
        !n.checks
            .watch(t0 + Duration::from_secs(180))
            .await
            .ip_changed
    );
}

#[tokio::test]
async fn enrolled_resets_the_checks() {
    let n = node(None);
    n.engine.set_health(false, None);
    let t0 = Instant::now();
    for i in 0..3 {
        n.checks.watch(t0 + Duration::from_secs(30 * i)).await;
    }
    assert_eq!(
        n.checks.watch(t0 + Duration::from_secs(90)).await.health,
        Health::Silent
    );
    n.checks.enrolled();
    n.engine.set_health(true, None);
    assert_eq!(
        n.checks.watch(t0 + Duration::from_secs(120)).await.health,
        Health::Good,
        "a node that has just enrolled starts with a clean slate"
    );
}
