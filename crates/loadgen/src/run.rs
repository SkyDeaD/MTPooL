//! The run: setup, simulated nodes, viewers, sampling and the report.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use mtpool_protocol::agent::{
    AgentConfig, Dc2Report, EnrollRequest, EnrollResponse, GpReport, HostReport, MetricsReport,
    ReportRequest,
};
use mtpool_protocol::public::{Engine, NodeState};
use rand::RngExt;
use reqwest::header::{HeaderMap, HeaderValue, IF_NONE_MATCH};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::MissedTickBehavior;

use crate::args::Args;
use crate::model::{self, StateCounts, Verdict};
use crate::procfs::{self, ProcSample};
use crate::stats::{Series, Summary};

const DOMAIN: &str = "pool.example.com";
const TLS_DOMAIN: &str = "www.example.com";
const PROXY_USER_SECRET: &str = "0123456789abcdef0123456789abcdef";
/// The value the stand's fakes accept as a service token; not a secret.
const FAKE_SECRET: &str = "e2e-fake-token";
/// Clock ticks per second of `/proc/<pid>/stat` on Linux.
const CLOCK_TICKS: f64 = 100.0;
const STATE_EVERY: Duration = Duration::from_secs(15);
const PROC_EVERY: Duration = Duration::from_secs(5);
/// The pages poll every 30 to 60 seconds with an ETag (specification, public pages).
const VIEWER_EVERY: Duration = Duration::from_secs(45);

/// Shared by every task.
struct Shared {
    args: Args,
    enroll_token: String,
    http_gp: reqwest::Client,
    steady: AtomicBool,
    steady_series: Mutex<BTreeMap<&'static str, Series>>,
    /// Errors over the whole run, steady or not.
    run_errors: Mutex<BTreeMap<&'static str, u64>>,
    enrolled: AtomicU64,
    enroll_failed: AtomicU64,
    gp_failures: AtomicU64,
    /// The intervals the registrar's config gave (the first node's; they are the same for all).
    intervals: std::sync::OnceLock<Value>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

struct Reply {
    status: u16,
    body: Vec<u8>,
    etag: Option<String>,
}

impl Shared {
    fn record(&self, kind: &'static str, took: Option<Duration>) {
        if took.is_none() {
            *lock(&self.run_errors).entry(kind).or_default() += 1;
        }
        if !self.steady.load(Ordering::Relaxed) {
            return;
        }
        let mut series = lock(&self.steady_series);
        let entry = series.entry(kind).or_default();
        match took {
            Some(d) => entry.ok(d),
            None => entry.failed(),
        }
    }

    /// Sends `request`, reads the whole body and records the time under `kind`. A transport error
    /// or a status that is not 2xx (nor 304 when `allow_304`) is an error. The kind may depend
    /// on the answer (`nodes_200` / `nodes_304`).
    async fn timed(
        &self,
        kind: impl Fn(u16) -> &'static str,
        error_kind: &'static str,
        request: reqwest::RequestBuilder,
        allow_304: bool,
    ) -> Option<Reply> {
        let started = Instant::now();
        let outcome = async {
            let response = request.send().await.ok()?;
            let status = response.status().as_u16();
            let etag = response
                .headers()
                .get(reqwest::header::ETAG)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let body = response.bytes().await.ok()?.to_vec();
            Some(Reply { status, body, etag })
        }
        .await;
        let took = started.elapsed();
        match outcome {
            Some(reply)
                if (200..300).contains(&reply.status) || (allow_304 && reply.status == 304) =>
            {
                self.record(kind(reply.status), Some(took));
                Some(reply)
            }
            Some(reply) => {
                eprintln!("{error_kind}: status {}", reply.status);
                self.record(error_kind, None);
                None
            }
            None => {
                self.record(error_kind, None);
                None
            }
        }
    }
}

fn client_for(ip: std::net::Ipv4Addr) -> Result<reqwest::Client, String> {
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-forwarded-for",
        HeaderValue::from_str(&ip.to_string()).map_err(|e| e.to_string())?,
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .no_proxy()
        .timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(1)
        .build()
        .map_err(|e| format!("cannot build the HTTP client: {e}"))
}

// --- setup --------------------------------------------------------------------------------------

async fn admin(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: Value,
) -> Result<Value, String> {
    let response = http
        .request(method, format!("{base}{path}"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("{path}: {}", e.without_url()))?;
    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("{path}: status {status}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))
}

/// Creates the owner and the enroll token and sets the pool up. The intervals stay at the
/// defaults, the ones the agent runs with in production.
async fn setup(args: &Args, http: &reqwest::Client) -> Result<String, String> {
    use reqwest::Method;
    let base = args.registrar.trim_end_matches('/');
    let token = &args.admin_token;
    admin(
        http,
        base,
        token,
        Method::PATCH,
        "/api/v1/admin/settings",
        json!({
            "domains": [DOMAIN],
            "tls_domain": TLS_DOMAIN,
            "proxy_users": [{"name": "load", "secret_hex": PROXY_USER_SECRET}],
            "secrets": {
                "globalping_token": FAKE_SECRET,
                "redflag_api_key": FAKE_SECRET,
                "cloudflare_api_token": FAKE_SECRET,
            },
        }),
    )
    .await?;
    let owner = admin(
        http,
        base,
        token,
        Method::POST,
        "/api/v1/admin/owners",
        json!({"name": "load", "node_limit": i64::from(args.nodes) + 10}),
    )
    .await?;
    let enroll = admin(
        http,
        base,
        token,
        Method::POST,
        "/api/v1/admin/tokens",
        json!({"kind": "enroll", "owner_id": owner["id"]}),
    )
    .await?;
    enroll["token"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "no token in the answer".to_owned())
}

// --- simulated node -----------------------------------------------------------------------------

struct Node {
    shared: Arc<Shared>,
    http: reqwest::Client,
    base: String,
    ip: std::net::Ipv4Addr,
    token: String,
    config: AgentConfig,
    gp_result: Mutex<Option<String>>,
    from_client: AtomicU64,
}

impl Node {
    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{}/api/v1/agent/{path}", self.base))
            .bearer_auth(&self.token)
    }

    async fn heartbeat(&self) {
        self.shared
            .timed(
                |_| "heartbeat",
                "heartbeat_error",
                self.post("heartbeat").json(&json!({})),
                false,
            )
            .await;
    }

    async fn sync(&self) {
        let request = self
            .http
            .get(format!("{}/api/v1/agent/config", self.base))
            .bearer_auth(&self.token);
        self.shared
            .timed(|_| "config", "config_error", request, false)
            .await;
    }

    async fn report(&self) {
        let gp = match lock(&self.gp_result).take() {
            Some(id) => GpReport::Measured { id },
            None => GpReport::NoData {
                reason: "no_data".into(),
            },
        };
        let sent = self.from_client.fetch_add(4096, Ordering::Relaxed);
        let body = ReportRequest {
            ip: self.ip,
            port: self.shared.args.node_port,
            sni: self.config.sni.clone(),
            gp,
            metrics: MetricsReport {
                ok: true,
                from_client_total: sent,
                to_client_total: sent * 3,
                clients: Some(0),
            },
            dc2: Some(Dc2Report {
                ok: true,
                reason: None,
            }),
            host: Some(HostReport {
                load1: 0.1,
                mem_used_bytes: 200_000_000,
                mem_total_bytes: 1_000_000_000,
                disk_used_bytes: 2_000_000_000,
                disk_total_bytes: 20_000_000_000,
            }),
            checked_at: mtpool_protocol::rfc3339(unix_ms()),
            error: None,
        };
        self.shared
            .timed(
                |_| "report",
                "report_error",
                self.post("report").json(&body),
                false,
            )
            .await;
    }

    /// One Globalping round as the agent does it: ask for a measurement, poll until it is no
    /// longer in progress, keep the id for the next report.
    async fn measure(&self) {
        let gp = self.shared.args.globalping.trim_end_matches('/');
        let body = model::measurement_body(self.ip, self.shared.args.node_port, &self.config.sni);
        let created = async {
            let r = self
                .shared
                .http_gp
                .post(format!("{gp}/v1/measurements"))
                .json(&body)
                .send()
                .await
                .ok()?;
            if !r.status().is_success() {
                return None;
            }
            r.json::<Value>().await.ok()?["id"]
                .as_str()
                .map(str::to_owned)
        }
        .await;
        let Some(id) = created else {
            self.shared.gp_failures.fetch_add(1, Ordering::Relaxed);
            return;
        };
        for _ in 0..120 {
            let state = async {
                let r = self
                    .shared
                    .http_gp
                    .get(format!("{gp}/v1/measurements/{id}"))
                    .send()
                    .await
                    .ok()?;
                r.json::<Value>().await.ok()?["status"]
                    .as_str()
                    .map(str::to_owned)
            }
            .await;
            match state.as_deref() {
                Some("in-progress") | None => tokio::time::sleep(Duration::from_millis(500)).await,
                Some(_) => {
                    *lock(&self.gp_result) = Some(id);
                    return;
                }
            }
        }
        self.shared.gp_failures.fetch_add(1, Ordering::Relaxed);
    }
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(0))
}

async fn every<F, Fut>(seconds: u32, mut job: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut tick = tokio::time::interval(Duration::from_secs(u64::from(seconds.max(1))));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        job().await;
    }
}

async fn run_node(shared: Arc<Shared>, index: u32, delay: Duration) {
    tokio::time::sleep(delay).await;
    let ip = model::node_ip(index);
    let Ok(http) = client_for(ip) else {
        shared.enroll_failed.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let base = shared.args.registrar.trim_end_matches('/').to_owned();

    let mut enrolled: Option<EnrollResponse> = None;
    for attempt in 0..5 {
        let body = EnrollRequest {
            name: format!("load{index}"),
            port: shared.args.node_port,
            engine: Engine::Telemt,
        };
        let request = http
            .post(format!("{base}/api/v1/agent/enroll"))
            .bearer_auth(&shared.enroll_token)
            .json(&body);
        if let Some(reply) = shared
            .timed(|_| "enroll", "enroll_error", request, false)
            .await
        {
            enrolled = serde_json::from_slice(&reply.body).ok();
            if enrolled.is_some() {
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(2 + attempt)).await;
    }
    let Some(enrolled) = enrolled else {
        shared.enroll_failed.fetch_add(1, Ordering::Relaxed);
        return;
    };
    shared.enrolled.fetch_add(1, Ordering::Relaxed);

    // The agent asks for its config before anything else.
    let request = http
        .get(format!("{base}/api/v1/agent/config"))
        .bearer_auth(&enrolled.node_token);
    let config = loop {
        match shared
            .timed(
                |_| "config",
                "config_error",
                request.try_clone().expect("no body"),
                false,
            )
            .await
        {
            Some(reply) => match serde_json::from_slice::<AgentConfig>(&reply.body) {
                Ok(config) => break config,
                Err(_) => return,
            },
            None => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    };
    let intervals = config.intervals;
    let _ = shared.intervals.set(json!({
        "heartbeat": intervals.heartbeat_sec,
        "globalping": intervals.globalping_sec,
        "metrics_report": intervals.metrics_sec,
        "config_sync": intervals.sync_sec,
        "source": "GET /agent/config of the registrar, its defaults",
    }));
    let node = Arc::new(Node {
        shared: shared.clone(),
        http,
        base,
        ip,
        token: enrolled.node_token,
        config,
        gp_result: Mutex::new(None),
        from_client: AtomicU64::new(0),
    });

    let n = node.clone();
    tokio::spawn(async move { every(intervals.heartbeat_sec, || n.heartbeat()).await });
    let n = node.clone();
    tokio::spawn(async move {
        // The first config was fetched above.
        tokio::time::sleep(Duration::from_secs(u64::from(intervals.sync_sec))).await;
        every(intervals.sync_sec, || n.sync()).await;
    });
    let n = node.clone();
    tokio::spawn(async move { every(intervals.metrics_sec, || n.report()).await });
    let n = node;
    tokio::spawn(async move { every(intervals.globalping_sec, || n.measure()).await });
}

// --- viewers ------------------------------------------------------------------------------------

async fn run_viewer(shared: Arc<Shared>, index: u32) {
    let Ok(http) = client_for(model::viewer_ip(index)) else {
        return;
    };
    let base = shared.args.registrar.trim_end_matches('/').to_owned();
    // Half of the viewers keep the ETag, as the web UI does; half always load the whole list.
    let conditional = index.is_multiple_of(2);
    let mut etag: Option<String> = None;
    let spread = rand::rng().random_range(0..VIEWER_EVERY.as_millis() as u64);
    tokio::time::sleep(Duration::from_millis(spread)).await;
    let mut round = 0u32;
    loop {
        let mut request = http.get(format!("{base}/api/v1/nodes"));
        if let (true, Some(tag)) = (conditional, &etag) {
            request = request.header(IF_NONE_MATCH, tag);
        }
        let kind = |status| {
            if status == 304 {
                "nodes_304"
            } else {
                "nodes_200"
            }
        };
        if let Some(reply) = shared.timed(kind, "nodes_error", request, true).await
            && reply.etag.is_some()
        {
            etag = reply.etag;
        }
        if round.is_multiple_of(3) {
            let request = http.get(format!("{base}/api/v1/domains"));
            shared
                .timed(|_| "domains", "domains_error", request, false)
                .await;
            let request = http.get(format!("{base}/api/v1/stats/summary"));
            shared
                .timed(|_| "stats_summary", "stats_summary_error", request, false)
                .await;
        }
        round += 1;
        tokio::time::sleep(VIEWER_EVERY).await;
    }
}

// --- sampling -----------------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct StatePoint {
    at_sec: u64,
    #[serde(flatten)]
    counts: StateCounts,
}

#[derive(Deserialize)]
struct StateOnly {
    state: NodeState,
}

async fn count_states(http: &reqwest::Client, base: &str) -> Option<StateCounts> {
    let body = http
        .get(format!("{base}/api/v1/nodes"))
        .send()
        .await
        .ok()?
        .bytes()
        .await
        .ok()?;
    let nodes: Vec<StateOnly> = serde_json::from_slice(&body).ok()?;
    let mut counts = StateCounts::default();
    for node in nodes {
        counts.add(node.state);
    }
    Some(counts)
}

// --- report -------------------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct RegistrarFigures {
    pid: Option<u32>,
    rss_kb_at_steady_start: Option<u64>,
    rss_kb_at_end: Option<u64>,
    rss_kb_max_sampled: Option<u64>,
    hwm_kb_at_end: Option<u64>,
    /// CPU seconds the registrar used during the measured window.
    cpu_seconds_in_window: Option<f64>,
    /// The same as a share of one core over the window.
    cpu_cores_in_window: Option<f64>,
}

#[derive(Debug, Serialize)]
struct Report {
    nodes: u32,
    duration_sec: u64,
    ramp_sec: u64,
    warmup_sec: u64,
    viewers: u32,
    intervals_sec: Value,
    nodes_enrolled: u64,
    nodes_enroll_failed: u64,
    globalping_failures: u64,
    /// Latency of each kind of request over the measured window.
    requests: BTreeMap<String, Summary>,
    /// `GET /api/v1/nodes`, 200 and 304 together.
    nodes_all: Summary,
    /// Errors over the whole run, including the ramp.
    errors_whole_run: BTreeMap<String, u64>,
    errors_in_window: u64,
    registrar: RegistrarFigures,
    states_at_steady_start: Option<StateCounts>,
    states_at_end: Option<StateCounts>,
    state_timeline: Vec<StatePoint>,
    verdict: Verdict,
}

/// Runs the whole thing and writes the report.
pub async fn run(args: Args) -> Result<(), String> {
    let admin_http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let enroll_token = if args.skip_setup {
        std::env::var("MTPOOL_ENROLL_TOKEN")
            .map_err(|_| "MTPOOL_ENROLL_TOKEN is not set".to_owned())?
    } else {
        setup(&args, &admin_http).await?
    };

    // The registrar probes `node_ip:port` over TCP: answer on every address of the loopback.
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", args.node_port))
        .await
        .map_err(|e| format!("cannot listen on port {}: {e}", args.node_port))?;
    tokio::spawn(async move {
        loop {
            // The probe only connects; the socket is closed at once.
            let _ = listener.accept().await;
        }
    });

    let shared = Arc::new(Shared {
        args: args.clone(),
        enroll_token,
        http_gp: reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| e.to_string())?,
        steady: AtomicBool::new(false),
        steady_series: Mutex::new(BTreeMap::new()),
        run_errors: Mutex::new(BTreeMap::new()),
        enrolled: AtomicU64::new(0),
        enroll_failed: AtomicU64::new(0),
        gp_failures: AtomicU64::new(0),
        intervals: std::sync::OnceLock::new(),
    });

    let started = Instant::now();
    for index in 0..args.nodes {
        let delay = Duration::from_millis(
            args.ramp * 1000 * u64::from(index) / u64::from(args.nodes.max(1)),
        );
        tokio::spawn(run_node(shared.clone(), index, delay));
    }
    for index in 0..args.viewers {
        tokio::spawn(run_viewer(shared.clone(), index));
    }

    let base = args.registrar.trim_end_matches('/').to_owned();
    let sampler_http = client_for(std::net::Ipv4Addr::new(198, 19, 0, 1))?;
    let timeline = Arc::new(Mutex::new(Vec::<StatePoint>::new()));
    let rss_max = Arc::new(AtomicU64::new(0));
    {
        let (timeline, base, http) = (timeline.clone(), base.clone(), sampler_http.clone());
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(STATE_EVERY);
            loop {
                tick.tick().await;
                if let Some(counts) = count_states(&http, &base).await {
                    lock(&timeline).push(StatePoint {
                        at_sec: started.elapsed().as_secs(),
                        counts,
                    });
                }
            }
        });
    }
    if let Some(pid) = args.registrar_pid {
        let rss_max = rss_max.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(PROC_EVERY);
            loop {
                tick.tick().await;
                if let Some(s) = procfs::read(pid) {
                    rss_max.fetch_max(s.rss_kb, Ordering::Relaxed);
                }
            }
        });
    }

    eprintln!(
        "ramp {} s, warmup {} s, then {} s measured",
        args.ramp, args.warmup, args.duration
    );
    tokio::time::sleep(Duration::from_secs(args.ramp + args.warmup)).await;
    let states_start = count_states(&sampler_http, &base).await;
    eprintln!(
        "steady state begins: enrolled {}, failed {}, states {:?}",
        shared.enrolled.load(Ordering::Relaxed),
        shared.enroll_failed.load(Ordering::Relaxed),
        states_start
    );
    let proc_start: Option<ProcSample> = args.registrar_pid.and_then(procfs::read);
    let window = Instant::now();
    shared.steady.store(true, Ordering::Relaxed);
    tokio::time::sleep(Duration::from_secs(args.duration)).await;
    shared.steady.store(false, Ordering::Relaxed);
    let window_secs = window.elapsed().as_secs_f64();
    let proc_end: Option<ProcSample> = args.registrar_pid.and_then(procfs::read);
    let states_end = count_states(&sampler_http, &base).await;

    let steady = lock(&shared.steady_series);
    let mut requests = BTreeMap::new();
    let mut errors_in_window = 0;
    for (kind, series) in steady.iter() {
        let summary = series.summary();
        errors_in_window += summary.errors;
        requests.insert((*kind).to_owned(), summary);
    }
    let nodes_all = Series::merged(&[
        steady.get("nodes_200").unwrap_or(&Series::default()),
        steady.get("nodes_304").unwrap_or(&Series::default()),
    ])
    .summary();
    drop(steady);
    let p99 = |kind: &str| requests.get(kind).and_then(|s| s.p99_ms);
    let verdict = model::verdict(p99("heartbeat"), p99("nodes_200"), errors_in_window);

    let cpu = match (proc_start, proc_end) {
        (Some(a), Some(b)) => Some(b.cpu_ticks.saturating_sub(a.cpu_ticks) as f64 / CLOCK_TICKS),
        _ => None,
    };
    let sampled_max = rss_max.load(Ordering::Relaxed);
    let report = Report {
        nodes: args.nodes,
        duration_sec: args.duration,
        ramp_sec: args.ramp,
        warmup_sec: args.warmup,
        viewers: args.viewers,
        intervals_sec: shared.intervals.get().cloned().unwrap_or(Value::Null),
        nodes_enrolled: shared.enrolled.load(Ordering::Relaxed),
        nodes_enroll_failed: shared.enroll_failed.load(Ordering::Relaxed),
        globalping_failures: shared.gp_failures.load(Ordering::Relaxed),
        requests,
        nodes_all,
        errors_whole_run: lock(&shared.run_errors)
            .iter()
            .map(|(k, v)| ((*k).to_owned(), *v))
            .collect(),
        errors_in_window,
        registrar: RegistrarFigures {
            pid: args.registrar_pid,
            rss_kb_at_steady_start: proc_start.map(|s| s.rss_kb),
            rss_kb_at_end: proc_end.map(|s| s.rss_kb),
            rss_kb_max_sampled: (sampled_max > 0).then_some(sampled_max),
            hwm_kb_at_end: proc_end.map(|s| s.hwm_kb),
            cpu_seconds_in_window: cpu,
            cpu_cores_in_window: cpu.map(|c| c / window_secs),
        },
        states_at_steady_start: states_start,
        states_at_end: states_end,
        state_timeline: std::mem::take(&mut *lock(&timeline)),
        verdict,
    };
    let text = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    match &args.report {
        Some(path) => {
            std::fs::write(path, text).map_err(|e| format!("cannot write the report: {e}"))?
        }
        None => println!("{text}"),
    }
    Ok(())
}
