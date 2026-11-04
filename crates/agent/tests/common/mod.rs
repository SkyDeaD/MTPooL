//! Shared helpers of the lifecycle tests: a registrar double (wiremock), memory state files and
//! recording hooks. The values here are made up for the tests.
#![allow(dead_code)]

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mtpool_agent::client::RegistrarClient;
use mtpool_agent::config::LocalConfig;
use mtpool_agent::engine::{
    ApplyOutcome, DesiredConfig, Engine as EngineTrait, EngineError, EngineHealth, EngineKind,
};
use mtpool_agent::lifecycle::{
    ConfigSink, DetectedEngine, Hooks, Jitter, Lifecycle, ReportSource, StaticEngine,
};
use mtpool_agent::local_health::PortCheck;
use mtpool_agent::state_files::{Fs, MemFs, StateFiles};
use mtpool_protocol::agent::{
    AgentConfig, AgentUser, Dc2Report, EnrollResponse, GpReport, Intervals, MetricsReport,
    ReportRequest, Terminate, TerminateReason,
};
use mtpool_protocol::public::Engine;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

pub const ENROLL_TOKEN: &str = "enroll-token-test";
pub const NODE_TOKEN: &str = "node-token-test";
pub const NEW_NODE_TOKEN: &str = "node-token-new";
pub const STATE_DIR: &str = "/state";

pub const ENROLL: &str = "/api/v1/agent/enroll";
pub const HEARTBEAT: &str = "/api/v1/agent/heartbeat";
pub const REPORT: &str = "/api/v1/agent/report";
pub const CONFIG: &str = "/api/v1/agent/config";
pub const RETIRE: &str = "/api/v1/agent/retire";

/// A jitter that always gives the same factor.
pub struct FixedJitter(pub f64);

impl Jitter for FixedJitter {
    fn factor(&mut self) -> f64 {
        self.0
    }
}

#[derive(Clone, Default)]
pub struct RecordingSink(pub Arc<Mutex<Vec<AgentConfig>>>);

impl RecordingSink {
    pub fn applied(&self) -> Vec<AgentConfig> {
        self.0.lock().unwrap().clone()
    }
}

impl ConfigSink for RecordingSink {
    async fn apply(&self, config: &AgentConfig) {
        self.0.lock().unwrap().push(config.clone());
    }
}

/// Builds a fixed report every time and counts the builds.
#[derive(Clone, Default)]
pub struct CountingReports(
    pub Arc<Mutex<u32>>,
    pub Arc<Mutex<bool>>,
    pub Arc<Mutex<bool>>,
);

impl CountingReports {
    pub fn built(&self) -> u32 {
        *self.0.lock().unwrap()
    }

    /// The checks find the proxy sick from now on.
    pub fn make_sick(&self) {
        *self.2.lock().unwrap() = true;
    }

    /// A measurement has finished: the next report carries it.
    pub fn finish_measurement(&self) {
        *self.1.lock().unwrap() = true;
    }
}

impl ReportSource for CountingReports {
    async fn build(&self) -> Option<ReportRequest> {
        *self.0.lock().unwrap() += 1;
        *self.1.lock().unwrap() = false;
        Some(sample_report())
    }

    async fn watch(&self, _now: tokio::time::Instant) -> mtpool_agent::lifecycle::Watch {
        let sick = *self.2.lock().unwrap();
        mtpool_agent::lifecycle::Watch {
            ip_changed: false,
            health: if sick {
                mtpool_agent::lifecycle::Health::Silent
            } else {
                mtpool_agent::lifecycle::Health::Good
            },
        }
    }

    fn has_measurement(&self) -> bool {
        *self.1.lock().unwrap()
    }
}

pub fn sample_report() -> ReportRequest {
    ReportRequest {
        ip: "203.0.113.7".parse().unwrap(),
        port: 443,
        sni: "tls.example.test".into(),
        gp: GpReport::NoData {
            reason: "rate_limited".into(),
        },
        metrics: MetricsReport {
            ok: true,
            from_client_total: 10,
            to_client_total: 20,
            clients: Some(1),
        },
        dc2: None,
        host: None,
        checked_at: "2026-11-03T12:00:00Z".into(),
        error: None,
    }
}

pub fn agent_config(heartbeat_sec: u32, sync_sec: u32) -> AgentConfig {
    AgentConfig {
        users: vec![AgentUser {
            name: "u1".into(),
            secret_hex: "00112233445566778899aabbccddeeff".into(),
        }],
        sni: "tls.example.test".into(),
        port: 443,
        intervals: Intervals {
            heartbeat_sec,
            globalping_sec: 300,
            metrics_sec: 60,
            sync_sec,
        },
        antiscan: true,
    }
}

pub fn enroll_response(node_token: &str) -> EnrollResponse {
    EnrollResponse {
        node_id: "alpha-a1b2c".into(),
        node_token: node_token.into(),
        generation: 1,
        replaced: None,
        reverify: false,
    }
}

pub fn terminate_body() -> Value {
    serde_json::to_value(Terminate {
        terminate: true,
        reason: TerminateReason::Banned,
        message: "the address is blocked".into(),
    })
    .unwrap()
}

pub fn problem_body(status: u16, code: &str) -> Value {
    json!({ "type": "about:blank", "title": code, "status": status, "code": code, "detail": "d" })
}

pub fn config_text(registrar: &str) -> String {
    format!(
        "registrar = \"{registrar}\"\nname = \"alpha\"\nstate_dir = \"{STATE_DIR}\"\nport = 443\n"
    )
}

pub fn local_config(registrar: &str) -> LocalConfig {
    LocalConfig::parse(&config_text(registrar), Path::new("agent.toml")).expect("valid config")
}

pub type TestLifecycle = Lifecycle<StaticEngine, CountingReports, RecordingSink>;

/// A registrar double with memory state files and recording hooks.
pub struct Harness {
    pub server: MockServer,
    pub fs: Arc<MemFs>,
    pub sink: RecordingSink,
    pub reports: CountingReports,
}

impl Harness {
    pub async fn new() -> Harness {
        Harness {
            server: MockServer::start().await,
            fs: Arc::new(MemFs::new()),
            sink: RecordingSink::default(),
            reports: CountingReports::default(),
        }
    }

    pub fn state(&self) -> StateFiles {
        StateFiles::new(self.fs.clone(), PathBuf::from(STATE_DIR))
    }

    pub fn put(&self, file: &str, value: &str) {
        self.fs
            .write_private(&Path::new(STATE_DIR).join(file), value.as_bytes())
            .unwrap();
    }

    pub fn get(&self, file: &str) -> Option<String> {
        self.fs
            .read(&Path::new(STATE_DIR).join(file))
            .unwrap()
            .map(|b| String::from_utf8(b).unwrap())
    }

    /// An enrolled node: id and token on disk, plus the enroll token.
    pub fn enrolled(&self) {
        self.put("node_id", "alpha-a1b2c");
        self.put("node_token", NODE_TOKEN);
        self.put("enroll_token", ENROLL_TOKEN);
    }

    /// Only the enroll token: the node is not enrolled yet.
    pub fn fresh(&self) {
        self.put("enroll_token", ENROLL_TOKEN);
    }

    pub fn lifecycle(&self) -> TestLifecycle {
        self.lifecycle_with_jitter(Box::new(FixedJitter(1.0)))
    }

    pub fn lifecycle_with_jitter(&self, jitter: Box<dyn Jitter>) -> TestLifecycle {
        let config = local_config(&self.server.uri());
        let client = RegistrarClient::new(config.registrar.clone()).unwrap();
        let hooks = Hooks {
            engine: StaticEngine(Some(DetectedEngine {
                engine: Engine::Telemt,
                port: 443,
            })),
            reports: self.reports.clone(),
            sink: self.sink.clone(),
        };
        Lifecycle::new(&config, client, self.fs.clone(), hooks, jitter)
            .expect("state files readable")
    }

    /// Answers `method` `route` with `template`, once if `once`.
    pub async fn mock(&self, verb: &str, route: &str, template: ResponseTemplate, once: bool) {
        let mock = Mock::given(method(verb))
            .and(path(route))
            .respond_with(template);
        let mock = if once { mock.up_to_n_times(1) } else { mock };
        mock.mount(&self.server).await;
    }

    pub async fn ok(&self, verb: &str, route: &str) {
        self.mock(
            verb,
            route,
            ResponseTemplate::new(200).set_body_json(json!({})),
            false,
        )
        .await;
    }

    /// How many requests reached `route`.
    pub async fn hits(&self, route: &str) -> usize {
        self.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == route)
            .count()
    }

    pub async fn total_requests(&self) -> usize {
        self.server.received_requests().await.unwrap().len()
    }
}

/// An engine that answers what it is told to.
pub struct FakeEngine {
    pub port: Mutex<Option<u16>>,
    pub health: Mutex<EngineHealth>,
    /// `None`: the engine does not answer.
    pub metrics: Mutex<Option<MetricsReport>>,
    pub dc2: Mutex<Option<Dc2Report>>,
}

impl FakeEngine {
    pub fn new() -> Arc<FakeEngine> {
        Arc::new(FakeEngine {
            port: Mutex::new(Some(443)),
            health: Mutex::new(EngineHealth {
                ok: true,
                reason: None,
            }),
            metrics: Mutex::new(Some(MetricsReport {
                ok: true,
                from_client_total: 10,
                to_client_total: 20,
                clients: Some(3),
            })),
            dc2: Mutex::new(Some(Dc2Report {
                ok: true,
                reason: None,
            })),
        })
    }

    pub fn set_health(&self, ok: bool, reason: Option<&str>) {
        *self.health.lock().unwrap() = EngineHealth {
            ok,
            reason: reason.map(str::to_owned),
        };
    }
}

#[async_trait]
impl EngineTrait for FakeEngine {
    fn kind(&self) -> EngineKind {
        EngineKind::Telemt
    }
    fn port(&self) -> Option<u16> {
        *self.port.lock().unwrap()
    }
    async fn metrics(&self) -> mtpool_agent::engine::Result<MetricsReport> {
        self.metrics
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| EngineError::Http("no answer".into()))
    }
    async fn dc2(&self) -> Option<Dc2Report> {
        self.dc2.lock().unwrap().clone()
    }
    async fn health(&self) -> EngineHealth {
        self.health.lock().unwrap().clone()
    }
    async fn apply_config(
        &self,
        _desired: &DesiredConfig,
    ) -> mtpool_agent::engine::Result<ApplyOutcome> {
        Ok(ApplyOutcome::Unchanged)
    }
}

/// A port check that says what it is told to and remembers what it was asked.
pub struct FakePorts {
    pub open: AtomicBool,
    pub asked: Mutex<Vec<(u16, Option<Ipv4Addr>)>>,
}

impl FakePorts {
    pub fn new(open: bool) -> Arc<FakePorts> {
        Arc::new(FakePorts {
            open: AtomicBool::new(open),
            asked: Mutex::new(Vec::new()),
        })
    }
    pub fn set(&self, open: bool) {
        self.open.store(open, Ordering::SeqCst);
    }
}

#[async_trait]
impl PortCheck for FakePorts {
    async fn listening(&self, port: u16, also: Option<Ipv4Addr>) -> bool {
        self.asked.lock().unwrap().push((port, also));
        self.open.load(Ordering::SeqCst)
    }
}
