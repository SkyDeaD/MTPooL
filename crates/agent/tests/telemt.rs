//! The telemt adapter: the Prometheus parser, the DC2 check and the configuration transaction,
//! all against the real telemt 3.5.16 answers recorded in `tests/fixtures/telemt` and scripted
//! hosts. Nothing here runs a command or touches the real file system.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use mtpool_agent::engine::dc2;
use mtpool_agent::engine::http::testing::ScriptedHttp;
use mtpool_agent::engine::http::{HttpGet, HttpResponse};
use mtpool_agent::engine::prom::{self, parse_prometheus};
use mtpool_agent::engine::telemt::{TelemtEngine, TelemtToml, metrics_url};
use mtpool_agent::engine::{ApplyOutcome, DesiredConfig, Engine, EngineError, EngineKind};
use mtpool_agent::host::testing::ScriptedRunner;
use mtpool_agent::lifecycle::{ConfigSink, DetectedEngine, EngineProbe};
use mtpool_agent::state_files::{Fs, MemFs};
use mtpool_protocol::agent::{AgentConfig, AgentUser, Dc2Report, Intervals};
use serde_json::{Value, json};

const METRICS: &str = include_str!("fixtures/telemt/metrics.txt");
const METRICS_USER_OFF: &str = include_str!("fixtures/telemt/metrics_user_telemetry_off.txt");
const DCS: &str = include_str!("fixtures/telemt/dcs.json");
const DCS_ME_DISABLED: &str = include_str!("fixtures/telemt/dcs_me_disabled.json");
const TOML: &str = include_str!("fixtures/telemt/telemt.toml");

const CONFIG: &str = "/etc/telemt/telemt.toml";
const BACKUP: &str = "/etc/telemt/telemt.toml.bak";
const STATE_DIR: &str = "/state";
const POOL_USERS: &str = "/state/pool_users.json";
const METRICS_URL: &str = "http://127.0.0.1:9090/metrics";
const DCS_URL: &str = "http://127.0.0.1:9091/v1/stats/dcs";
/// The second the recorded `dcs.json` was generated at.
const DCS_AT: u64 = 1_793_732_062;

const ALICE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const BOB: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const OWNER: &str = "cccccccccccccccccccccccccccccccc";

fn ok(body: &str) -> Result<HttpResponse, String> {
    Ok(HttpResponse {
        status: 200,
        body: body.to_owned(),
    })
}

/// The recorded config with the control API switched off: telemt answers on it by default, so
/// the only way to have none is `enabled = false`.
fn without_api() -> String {
    let off = TOML.replace(
        "[server.api]\nenabled = true\n",
        "[server.api]\nenabled = false\n",
    );
    assert_ne!(off, TOML);
    off
}

/// The recorded config with the owner's own user and a comment of the owner's added.
fn owner_config() -> String {
    format!("{TOML}owner = \"{OWNER}\" # the owner's own user, not the pool's\n")
}

fn desired(users: &[(&str, &str)], sni: &str) -> DesiredConfig {
    DesiredConfig {
        users: users
            .iter()
            .map(|(name, secret)| ((*name).to_owned(), (*secret).to_owned()))
            .collect(),
        sni: sni.to_owned(),
    }
}

struct Rig<H: HttpGet + 'static> {
    fs: Arc<MemFs>,
    runner: Arc<ScriptedRunner>,
    http: Arc<H>,
    engine: TelemtEngine<ScriptedRunner, H>,
}

impl<H: HttpGet + 'static> Rig<H> {
    fn config_text(&self) -> String {
        String::from_utf8(self.fs.read(Path::new(CONFIG)).unwrap().unwrap()).unwrap()
    }
    fn file(&self, path: &str) -> Option<String> {
        self.fs
            .read(Path::new(path))
            .unwrap()
            .map(|bytes| String::from_utf8(bytes).unwrap())
    }
    /// The names the pool remembers: the state file of an older agent is a bare list, a newer
    /// one an object with the list under `users`.
    fn pool_users(&self) -> Option<Vec<String>> {
        let value: Value = serde_json::from_str(&self.file(POOL_USERS)?).unwrap();
        let list = if value.is_array() {
            value
        } else {
            value["users"].clone()
        };
        Some(serde_json::from_value(list).unwrap())
    }
    /// The TLS domain the pool remembers it added.
    fn pool_sni(&self) -> Option<String> {
        let value: Value = serde_json::from_str(&self.file(POOL_USERS)?).unwrap();
        value["sni"].as_str().map(str::to_owned)
    }
    /// The `systemctl` calls as `verb unit` strings.
    fn systemctl(&self) -> Vec<String> {
        self.runner
            .calls()
            .iter()
            .map(|call| {
                assert_eq!(call.program, "systemctl");
                call.args.join(" ")
            })
            .collect()
    }
}

fn rig_with<H: HttpGet + 'static>(
    config: Option<&str>,
    state: Option<&str>,
    http: impl FnOnce(&Arc<MemFs>) -> H,
) -> Rig<H> {
    let fs = Arc::new(MemFs::new());
    if let Some(config) = config {
        fs.write_private(Path::new(CONFIG), config.as_bytes())
            .unwrap();
    }
    if let Some(state) = state {
        fs.write_private(Path::new(POOL_USERS), state.as_bytes())
            .unwrap();
    }
    let runner = Arc::new(ScriptedRunner::new());
    let http = Arc::new(http(&fs));
    let engine = TelemtEngine::new(
        CONFIG.into(),
        "telemt.service",
        STATE_DIR.into(),
        runner.clone(),
        fs.clone(),
        http.clone(),
    )
    .with_clock(|| DCS_AT + 10);
    Rig {
        fs,
        runner,
        http,
        engine,
    }
}

fn rig(config: &str, state: Option<&str>) -> Rig<ScriptedHttp> {
    rig_with(Some(config), state, |_| {
        let http = ScriptedHttp::new();
        http.always(METRICS_URL, ok(METRICS));
        http
    })
}

/// `systemctl stop` and `start` both succeed.
fn script_restart(runner: &ScriptedRunner) {
    runner.push_output(0, b"");
    runner.push_output(0, b"");
}

// ---------------------------------------------------------------------------------------------
// The Prometheus parser and the metrics report
// ---------------------------------------------------------------------------------------------

#[test]
fn parses_writers_and_user_metrics() {
    let samples = parse_prometheus(METRICS);
    assert!(samples.len() > 500, "{} samples", samples.len());
    let build = samples
        .iter()
        .find(|s| s.name == "telemt_build_info")
        .unwrap();
    assert_eq!(build.label("version"), Some("3.5.16"));
    assert_eq!(
        prom::first(&samples, "telemt_me_writers_active_current"),
        Some(0.0)
    );
    assert_eq!(
        prom::sum(&samples, "telemt_user_unique_ips_current"),
        Some(0.0)
    );
    assert_eq!(prom::first(&samples, "telemt_no_such_metric"), None);
    assert_eq!(prom::sum(&samples, "telemt_no_such_metric"), None);

    // The recorded run had no client traffic, so per-user series with traffic are the format of
    // the recorded series with numbers in them.
    let text = r#"
# HELP telemt_me_writers_active_current Current non-draining active ME writers
# TYPE telemt_me_writers_active_current gauge
telemt_me_writers_active_current 3
telemt_user_unique_ips_current{user="alice"} 2
telemt_user_unique_ips_current{user="bob"} 1
telemt_user_octets_from_client_total{user="alice"} 1500
telemt_user_octets_from_client_total{user="bob"} 500
telemt_user_octets_to_client_total{user="alice"} 9000
telemt_user_octets_to_client_total{user="bob"} 1000
"#;
    let samples = parse_prometheus(text);
    assert_eq!(
        prom::first(&samples, "telemt_me_writers_active_current"),
        Some(3.0)
    );
    assert_eq!(
        prom::sum(&samples, "telemt_user_unique_ips_current"),
        Some(3.0)
    );
    assert_eq!(
        samples
            .iter()
            .filter(|s| s.name == "telemt_user_octets_from_client_total")
            .map(|s| s.label("user").unwrap())
            .collect::<Vec<_>>(),
        ["alice", "bob"]
    );
}

#[test]
fn split_sum_goes_by_the_label() {
    let samples = parse_prometheus(concat!(
        "m{user=\"a\"} 1\n",
        "m{user=\"b\"} 2\n",
        "m{user=\"c\"} 4\n",
        "m{user=\"d\"} NaN\n",
        "m 8\n",
        "n{user=\"a\"} 16\n",
    ));
    let pool = ["a".to_owned(), "b".to_owned(), "absent".to_owned()];
    // A series without the label is nobody the pool set up.
    assert_eq!(
        prom::split_sum(&samples, "m", "user", &pool),
        Some((3.0, 12.0))
    );
    assert_eq!(
        prom::split_sum(&samples, "n", "user", &[]),
        Some((0.0, 16.0))
    );
    assert_eq!(prom::split_sum(&samples, "absent", "user", &pool), None);
}

#[test]
fn parser_handles_escapes_specials_timestamps_and_noise() {
    let text = concat!(
        "# a comment\n",
        "\n",
        "plain 1.5\n",
        "with_ts 7 1700000000000\n",
        "escaped{user=\"a\\\"b\\\\c\\nd\",zone=\"x\"} 2\n",
        "spaced { k = \"v\" } 3\n",
        "nan_value NaN\n",
        "inf_value +Inf\n",
        "exp 1e3\n",
        "broken{user=\"unterminated} 4\n",
        "no_value\n",
        "bad_value abc\n",
        "trailing 5\r\n",
    );
    let samples = parse_prometheus(text);
    let by = |name: &str| samples.iter().find(|s| s.name == name).unwrap();
    assert_eq!(by("plain").value, 1.5);
    assert_eq!(by("with_ts").value, 7.0);
    let escaped = by("escaped");
    assert_eq!(escaped.label("user"), Some("a\"b\\c\nd"));
    assert_eq!(escaped.label("zone"), Some("x"));
    assert_eq!(escaped.value, 2.0);
    assert_eq!(by("spaced").label("k"), Some("v"));
    assert!(by("nan_value").value.is_nan());
    assert_eq!(by("inf_value").value, f64::INFINITY);
    assert_eq!(by("exp").value, 1000.0);
    assert_eq!(by("trailing").value, 5.0);
    for name in ["broken", "no_value", "bad_value"] {
        assert!(
            samples.iter().all(|s| s.name != name),
            "{name} is not a sample"
        );
    }
    // A sum never turns into NaN because of one bad series.
    let with_nan = parse_prometheus("m 1\nm NaN\nm 2\n");
    assert_eq!(prom::sum(&with_nan, "m"), Some(3.0));
}

fn with_users(extra: &str) -> String {
    format!("{METRICS}{extra}")
}

#[tokio::test]
async fn metrics_report_from_the_recorded_answer() {
    let rig = rig(TOML, Some(r#"["probe"]"#));
    let report = rig.engine.metrics().await.unwrap();
    // The recording has no client traffic and one user without a connection.
    assert_eq!(report.from_client_total, 0);
    assert_eq!(report.to_client_total, 0);
    assert_eq!(report.clients, Some(0));
    assert_eq!(report.other_clients, Some(0));
    assert_eq!(report.other_from_client_total, Some(0));
    assert_eq!(report.other_to_client_total, Some(0));
    assert!(report.ok);
    assert_eq!(rig.http.requests().len(), 1);
    assert_eq!(rig.http.requests()[0].url, METRICS_URL);
    assert_eq!(rig.http.requests()[0].authorization, None);
}

#[tokio::test]
async fn octets_with_and_without_total_suffix() {
    let with_total = with_users(concat!(
        "telemt_user_octets_from_client_total{user=\"alice\"} 1500\n",
        "telemt_user_octets_from_client_total{user=\"bob\"} 500\n",
        "telemt_user_octets_to_client_total{user=\"alice\"} 9000\n",
        "telemt_user_octets_to_client_total{user=\"bob\"} 1000\n",
        "telemt_user_unique_ips_current{user=\"alice\"} 2\n",
        "telemt_user_unique_ips_current{user=\"bob\"} 1\n",
    ));
    let without_total = with_users(concat!(
        "telemt_user_octets_from_client{user=\"alice\"} 1500\n",
        "telemt_user_octets_from_client{user=\"bob\"} 500\n",
        "telemt_user_octets_to_client{user=\"alice\"} 9000\n",
        "telemt_user_octets_to_client{user=\"bob\"} 1000\n",
        "telemt_user_unique_ips_current{user=\"alice\"} 2\n",
        "telemt_user_unique_ips_current{user=\"bob\"} 1\n",
    ));
    // An engine that exported both spellings would be counted once.
    let both = format!(
        "{with_total}telemt_user_octets_from_client{{user=\"alice\"}} 1500\ntelemt_user_octets_to_client{{user=\"alice\"}} 9000\n"
    );
    for body in [with_total, without_total, both] {
        let rig = rig_with(Some(TOML), Some(r#"["alice","bob"]"#), |_| {
            let http = ScriptedHttp::new();
            http.always(METRICS_URL, ok(&body));
            http
        });
        let report = rig.engine.metrics().await.unwrap();
        assert_eq!(report.from_client_total, 2000, "{body:.0}");
        assert_eq!(report.to_client_total, 10_000);
        assert_eq!(report.clients, Some(3));
        assert!(report.ok);
    }
}

/// Two users with live traffic. NOT a recording: no recorded node has a second user with traffic,
/// so the numbers are made up; the series are those of `fixtures/mtproxyl/metrics_users.txt`.
const POOL_AND_OWNER_WITH_TRAFFIC: &str = concat!(
    "telemt_user_octets_from_client_total{user=\"alice\"} 1500\n",
    "telemt_user_octets_to_client_total{user=\"alice\"} 9000\n",
    "telemt_user_unique_ips_current{user=\"alice\"} 2\n",
    "telemt_user_octets_from_client_total{user=\"owner\"} 500\n",
    "telemt_user_octets_to_client_total{user=\"owner\"} 1000\n",
    "telemt_user_unique_ips_current{user=\"owner\"} 7\n",
);

fn rig_on_body(pool_users: Option<&str>, body: &str) -> Rig<ScriptedHttp> {
    rig_with(Some(TOML), pool_users, |_| {
        let http = ScriptedHttp::new();
        http.always(METRICS_URL, ok(body));
        http
    })
}

#[tokio::test]
async fn the_pools_users_are_counted_apart_from_the_owners() {
    let body = with_users(POOL_AND_OWNER_WITH_TRAFFIC);
    let rig = rig_on_body(Some(r#"["alice"]"#), &body);
    let report = rig.engine.metrics().await.unwrap();
    assert_eq!(report.from_client_total, 1500);
    assert_eq!(report.to_client_total, 9000);
    assert_eq!(report.clients, Some(2));
    assert_eq!(report.other_from_client_total, Some(500));
    assert_eq!(report.other_to_client_total, Some(1000));
    // The recording's own user `probe` has no client and counts among the others too.
    assert_eq!(report.other_clients, Some(7));
}

#[tokio::test]
async fn a_pool_user_without_a_series_counts_as_zero() {
    let body = with_users(POOL_AND_OWNER_WITH_TRAFFIC);
    let rig = rig_on_body(Some(r#"["carol"]"#), &body);
    let report = rig.engine.metrics().await.unwrap();
    assert_eq!(report.from_client_total, 0);
    assert_eq!(report.to_client_total, 0);
    assert_eq!(report.clients, Some(0));
    assert_eq!(report.other_from_client_total, Some(2000));
    assert_eq!(report.other_to_client_total, Some(10_000));
    assert_eq!(report.other_clients, Some(9));
}

#[tokio::test]
async fn a_pool_with_no_users_leaves_everything_to_the_others() {
    let body = with_users(POOL_AND_OWNER_WITH_TRAFFIC);
    let rig = rig_on_body(Some(r#"{"users":[],"sni":"example.com"}"#), &body);
    let report = rig.engine.metrics().await.unwrap();
    assert_eq!(report.from_client_total, 0);
    assert_eq!(report.clients, Some(0));
    assert_eq!(report.other_from_client_total, Some(2000));
    assert_eq!(report.other_clients, Some(9));
}

#[tokio::test]
async fn nothing_is_counted_until_the_pools_users_are_known() {
    let body = with_users(POOL_AND_OWNER_WITH_TRAFFIC);
    // No state file yet, and one that makes no sense: nobody can say whose traffic this is. The
    // report has zeros and no split, which the registrar takes as no reading to count from.
    for state in [None, Some("not json")] {
        let rig = rig_on_body(state, &body);
        let report = rig.engine.metrics().await.unwrap();
        assert!(report.ok);
        assert_eq!(report.from_client_total, 0);
        assert_eq!(report.to_client_total, 0);
        assert_eq!(report.clients, None);
        assert_eq!(report.other_clients, None);
        assert_eq!(report.other_from_client_total, None);
        assert_eq!(report.other_to_client_total, None);
    }
}

#[tokio::test]
async fn the_list_follows_the_applied_config_and_survives_a_restart() {
    let body = with_users(POOL_AND_OWNER_WITH_TRAFFIC);
    let rig = rig_on_body(None, &body);
    assert_eq!(rig.engine.metrics().await.unwrap().clients, None);

    script_restart(&rig.runner);
    let applied = rig
        .engine
        .apply_config(&desired(&[("alice", ALICE)], "example.com"))
        .await
        .unwrap();
    assert_eq!(applied, ApplyOutcome::Applied);
    assert_eq!(rig.engine.metrics().await.unwrap().clients, Some(2));

    // The pool swaps its user: the next look counts the new one.
    script_restart(&rig.runner);
    rig.engine
        .apply_config(&desired(&[("bob", BOB)], "example.com"))
        .await
        .unwrap();
    let report = rig.engine.metrics().await.unwrap();
    assert_eq!(report.clients, Some(0));
    assert_eq!(report.other_clients, Some(9));

    // A new process on the same state directory knows the list without a sync.
    let restarted = TelemtEngine::new(
        CONFIG.into(),
        "telemt.service",
        STATE_DIR.into(),
        rig.runner.clone(),
        rig.fs.clone(),
        rig.http.clone(),
    );
    let report = restarted.metrics().await.unwrap();
    assert_eq!(report.clients, Some(0));
    assert_eq!(report.other_from_client_total, Some(2000));
}

#[tokio::test]
async fn metrics_ok_follows_the_middle_proxy_writers_without_a_control_api() {
    let no_api = without_api();
    assert!(!TelemtToml::parse(&no_api).unwrap().control_api().is_some());

    // Writers at 0 and nothing else to say how telemt does: not serving.
    let rig_down = rig(&no_api, None);
    assert!(!rig_down.engine.metrics().await.unwrap().ok);

    let serving = METRICS.replace(
        "telemt_me_writers_active_current 0",
        "telemt_me_writers_active_current 2",
    );
    let rig_up = rig_with(Some(&no_api), None, |_| {
        let http = ScriptedHttp::new();
        http.always(METRICS_URL, ok(&serving));
        http
    });
    assert!(rig_up.engine.metrics().await.unwrap().ok);

    // Without the middle proxy there are no writers to wait for.
    let direct = no_api.replace("use_middle_proxy = true", "use_middle_proxy = false");
    assert!(rig(&direct, None).engine.metrics().await.unwrap().ok);
}

#[tokio::test]
async fn metrics_errors_are_reported_not_hidden() {
    // No answer at all.
    let rig_down = rig_with(Some(TOML), None, |_| ScriptedHttp::new());
    assert!(matches!(
        rig_down.engine.metrics().await,
        Err(EngineError::Http(_))
    ));
    // An error status is not metrics.
    let rig_500 = rig_with(Some(TOML), None, |_| {
        let http = ScriptedHttp::new();
        http.always(
            METRICS_URL,
            Ok(HttpResponse {
                status: 503,
                body: METRICS.to_owned(),
            }),
        );
        http
    });
    assert!(matches!(
        rig_500.engine.metrics().await,
        Err(EngineError::Http(_))
    ));
    // A page that is not metrics.
    let rig_html = rig_with(Some(TOML), None, |_| {
        let http = ScriptedHttp::new();
        http.always(METRICS_URL, ok("<html>hello</html>"));
        http
    });
    assert!(matches!(
        rig_html.engine.metrics().await,
        Err(EngineError::Http(_))
    ));
    // No configuration, no engine.
    let rig_none = rig_with(None, None, |_| ScriptedHttp::new());
    assert!(matches!(
        rig_none.engine.metrics().await,
        Err(EngineError::Config(_))
    ));
}

#[test]
fn metrics_url_listen_over_port_over_default_9090() {
    let url = |toml: &str| metrics_url(&TelemtToml::parse(toml).unwrap());
    // `[server] metrics_listen` wins over `metrics_port`.
    assert_eq!(
        url("[server]\nmetrics_listen = \"127.0.0.1:9188\"\nmetrics_port = 9100\n"),
        "http://127.0.0.1:9188/metrics"
    );
    assert_eq!(
        url("[server]\nmetrics_listen = \"10.1.2.3:9188\"\n"),
        "http://10.1.2.3:9188/metrics"
    );
    // A wildcard bind is reached on the loopback.
    assert_eq!(
        url("[server]\nmetrics_listen = \"0.0.0.0:9188\"\n"),
        "http://127.0.0.1:9188/metrics"
    );
    assert_eq!(
        url("[server]\nmetrics_listen = \"[::]:9188\"\n"),
        "http://[::1]:9188/metrics"
    );
    // `metrics_port` is served on the loopback.
    assert_eq!(
        url("[server]\nmetrics_port = 9100\n"),
        "http://127.0.0.1:9100/metrics"
    );
    // Neither: the default.
    assert_eq!(
        url("[server]\nport = 8443\n"),
        "http://127.0.0.1:9090/metrics"
    );
    assert_eq!(url(""), "http://127.0.0.1:9090/metrics");
    // The recorded config.
    assert_eq!(url(TOML), METRICS_URL);
    // A listen address that is not an address is not used.
    assert_eq!(
        url("[server]\nmetrics_listen = \"not an address\"\nmetrics_port = 9100\n"),
        "http://127.0.0.1:9100/metrics"
    );
}

#[test]
fn telemt_toml_reads_what_the_agent_needs() {
    let toml = TelemtToml::parse(TOML).unwrap();
    assert_eq!(toml.port(), 8443);
    assert_eq!(toml.tls_domain().as_deref(), Some("example.com"));
    assert_eq!(
        toml.users(),
        [(
            "probe".to_owned(),
            "00112233445566778899aabbccddeeff".to_owned()
        )]
    );
    assert!(toml.user_telemetry_enabled());
    let api = toml.control_api().unwrap();
    assert_eq!(api.base, "http://127.0.0.1:9091");
    assert_eq!(api.authorization, None);

    // The defaults of telemt: port 443, user telemetry on, a control API only when the table is
    // there.
    let bare = TelemtToml::parse("").unwrap();
    assert_eq!(bare.port(), 443);
    assert_eq!(bare.tls_domain(), None);
    assert!(bare.users().is_empty());
    assert!(bare.user_telemetry_enabled());
    // telemt serves its control API on 0.0.0.0:9091 unless it is switched off.
    let api = bare.control_api().unwrap();
    assert_eq!(api.base, "http://127.0.0.1:9091");
    assert_eq!(api.authorization, None);

    let off = TelemtToml::parse("[general.telemetry]\nuser_enabled = false\n").unwrap();
    assert!(!off.user_telemetry_enabled());

    // The legacy name of the table, a wildcard bind and a header.
    let legacy = TelemtToml::parse(
        "[server.admin_api]\nlisten = \"0.0.0.0:9191\"\nauth_header = \"Bearer test-value\"\n",
    )
    .unwrap();
    let api = legacy.control_api().unwrap();
    assert_eq!(api.base, "http://127.0.0.1:9191");
    assert_eq!(api.authorization.as_deref(), Some("Bearer test-value"));
    // An empty header means no header; a disabled API is not configured.
    let empty = TelemtToml::parse("[server.api]\nauth_header = \"\"\n").unwrap();
    assert_eq!(empty.control_api().unwrap().authorization, None);
    assert_eq!(empty.control_api().unwrap().base, "http://127.0.0.1:9091");
    let disabled = TelemtToml::parse("[server.api]\nenabled = false\n").unwrap();
    assert!(disabled.control_api().is_none());

    assert!(matches!(
        TelemtToml::parse("[server\nport ="),
        Err(EngineError::Config(_))
    ));
}

// ---------------------------------------------------------------------------------------------
// DC2
// ---------------------------------------------------------------------------------------------

/// The recorded `dcs.json` changed by `edit`.
fn dcs_with(edit: impl FnOnce(&mut Value)) -> String {
    let mut value: Value = serde_json::from_str(DCS).unwrap();
    edit(&mut value);
    value.to_string()
}

fn dc2_entry(value: &mut Value) -> &mut Value {
    value["data"]["dcs"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|dc| dc["dc"] == 2)
        .unwrap()
}

fn healthy_dcs() -> String {
    dcs_with(|value| {
        let dc = dc2_entry(value);
        dc["available_endpoints"] = json!(1);
        dc["alive_writers"] = json!(2);
        dc["fresh_alive_writers"] = json!(2);
    })
}

#[test]
fn dc2_reasons() {
    let at = DCS_AT;
    let cases: Vec<(&str, Result<String, ()>, u64, Dc2Report)> = vec![
        (
            "healthy",
            Ok(healthy_dcs()),
            at,
            Dc2Report {
                ok: true,
                reason: None,
            },
        ),
        ("the api did not answer", Err(()), at, fail(dc2::API_ERROR)),
        (
            "the answer is not json",
            Ok("<html>".into()),
            at,
            fail(dc2::API_ERROR),
        ),
        (
            "the api says not ok",
            Ok(dcs_with(|v| v["ok"] = json!(false))),
            at,
            fail(dc2::API_ERROR),
        ),
        (
            "no data in the answer",
            Ok(r#"{"ok":true}"#.into()),
            at,
            fail(dc2::API_ERROR),
        ),
        (
            "middle proxy is off (recorded answer)",
            Ok(DCS_ME_DISABLED.into()),
            1_793_732_129,
            fail(dc2::ME_DISABLED),
        ),
        (
            "snapshot a second over 300 old",
            Ok(healthy_dcs()),
            at + 301,
            fail(dc2::STALE),
        ),
        (
            "snapshot exactly 300 old is still good",
            Ok(healthy_dcs()),
            at + 300,
            Dc2Report {
                ok: true,
                reason: None,
            },
        ),
        (
            "snapshot a second over 120 ahead",
            Ok(healthy_dcs()),
            at - 121,
            fail(dc2::STALE),
        ),
        (
            "snapshot exactly 120 ahead is still good",
            Ok(healthy_dcs()),
            at - 120,
            Dc2Report {
                ok: true,
                reason: None,
            },
        ),
        (
            "no time in the snapshot",
            Ok(dcs_with(|v| {
                v["data"]
                    .as_object_mut()
                    .unwrap()
                    .remove("generated_at_epoch_secs");
            })),
            at,
            fail(dc2::STALE),
        ),
        (
            "dc 2 is not in the answer",
            Ok(dcs_with(|v| {
                v["data"]["dcs"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|dc| dc["dc"] != 2);
            })),
            at,
            fail(dc2::ABSENT),
        ),
        (
            "dc 2 has no endpoints",
            Ok(dcs_with(|v| dc2_entry(v)["endpoints"] = json!([]))),
            at,
            fail(dc2::NO_ENDPOINTS),
        ),
        (
            "dc 2 has no fresh writer",
            Ok(dcs_with(|v| {
                let dc = dc2_entry(v);
                dc["fresh_alive_writers"] = json!(0);
                dc["alive_writers"] = json!(2);
            })),
            at,
            fail(dc2::NO_FRESH),
        ),
        (
            "the recorded answer: endpoints known, nothing alive",
            Ok(DCS.into()),
            at,
            fail(dc2::NO_FRESH),
        ),
    ];
    for (what, answer, now, expected) in cases {
        let body = match &answer {
            Ok(body) => Ok(body.as_str()),
            Err(()) => Err(()),
        };
        let got = dc2::evaluate(body, now);
        assert_eq!(got, expected, "{what}");
    }
    // The strings the registrar and the spec know.
    assert_eq!(
        [
            dc2::API_ERROR,
            dc2::ME_DISABLED,
            dc2::STALE,
            dc2::ABSENT,
            dc2::NO_ENDPOINTS,
            dc2::NO_FRESH
        ],
        [
            "dc2_api_error",
            "dc2_me_disabled",
            "dc2_stale",
            "dc2_absent",
            "dc2_no_endpoints",
            "dc2_no_fresh"
        ]
    );
}

fn fail(reason: &str) -> Dc2Report {
    Dc2Report {
        ok: false,
        reason: Some(reason.to_owned()),
    }
}

#[tokio::test]
async fn dc2_asks_the_control_api_with_its_header() {
    let config = TOML.replace(
        "listen = \"127.0.0.1:9091\"\n",
        "listen = \"127.0.0.1:9091\"\nauth_header = \"Bearer test-value\"\n",
    );
    let rig = rig_with(Some(&config), None, |_| {
        let http = ScriptedHttp::new();
        http.always(DCS_URL, ok(&healthy_dcs()));
        http
    });
    let report = rig.engine.dc2().await.unwrap();
    assert_eq!(
        report,
        Dc2Report {
            ok: true,
            reason: None
        }
    );
    let requests = rig.http.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, DCS_URL);
    assert_eq!(
        requests[0].authorization.as_deref(),
        Some("Bearer test-value")
    );
}

#[tokio::test]
async fn dc2_failures_of_the_api_are_a_reason_not_an_error() {
    let down = rig_with(Some(TOML), None, |_| ScriptedHttp::new());
    assert_eq!(down.engine.dc2().await, Some(fail(dc2::API_ERROR)));
    let forbidden = rig_with(Some(TOML), None, |_| {
        let http = ScriptedHttp::new();
        http.always(
            DCS_URL,
            Ok(HttpResponse {
                status: 401,
                body: healthy_dcs(),
            }),
        );
        http
    });
    assert_eq!(forbidden.engine.dc2().await, Some(fail(dc2::API_ERROR)));
}

#[tokio::test]
async fn dc2_asks_the_default_control_api_when_the_config_has_no_table() {
    let bare = "[server]\nport = 8443\nmetrics_port = 9090\n\n[access.users]\nprobe = \"00112233445566778899aabbccddeeff\"\n";
    assert!(!bare.contains("api"));
    let rig = rig_with(Some(bare), None, |_| {
        let http = ScriptedHttp::new();
        http.always(DCS_URL, ok(&healthy_dcs()));
        http
    });
    assert_eq!(
        rig.engine.dc2().await,
        Some(Dc2Report {
            ok: true,
            reason: None
        })
    );
    let requests = rig.http.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, DCS_URL);
    assert_eq!(requests[0].authorization, None);
}

#[tokio::test]
async fn dc2_is_not_checked_without_a_control_api() {
    let no_api = without_api();
    let rig = rig(&no_api, None);
    assert_eq!(rig.engine.dc2().await, None);
    assert!(rig.http.requests().is_empty());
    // No configuration: nothing to ask either.
    let none = rig_with(None, None, |_| ScriptedHttp::new());
    assert_eq!(none.engine.dc2().await, None);
}

// ---------------------------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn user_telemetry_disabled_is_reported() {
    let config = TOML.replace("user_enabled = true", "user_enabled = false");
    let rig = rig_with(Some(&config), Some(r#"["probe"]"#), |_| {
        let http = ScriptedHttp::new();
        http.always(METRICS_URL, ok(METRICS_USER_OFF));
        http
    });
    // Without per-user statistics there are no clients and no traffic to report.
    let report = rig.engine.metrics().await.unwrap();
    assert!(report.ok);
    assert_eq!(report.clients, None);
    assert_eq!(report.from_client_total, 0);
    assert_eq!(report.to_client_total, 0);
    assert_eq!(report.other_clients, None);
    assert_eq!(report.other_from_client_total, None);
    assert_eq!(report.other_to_client_total, None);
    // The engine works, but something is lost: ok with a reason.
    let health = rig.engine.health().await;
    assert!(health.ok);
    assert_eq!(health.reason.as_deref(), Some("user_telemetry_disabled"));

    // The engine's own switch is what counts: the gauge in `/metrics` says it is off even if the
    // file the agent read says it is on.
    let rig_gauge = rig_with(Some(TOML), None, |_| {
        let http = ScriptedHttp::new();
        http.always(METRICS_URL, ok(METRICS_USER_OFF));
        http
    });
    assert_eq!(rig_gauge.engine.metrics().await.unwrap().clients, None);
    assert_eq!(
        rig_gauge.engine.health().await.reason.as_deref(),
        Some("user_telemetry_disabled")
    );

    // With it on there is nothing to warn about.
    let healthy = self::rig(TOML, None).engine.health().await;
    assert!(healthy.ok);
    assert_eq!(healthy.reason, None);
}

#[tokio::test]
async fn health_is_not_ok_when_metrics_do_not_answer_or_no_writers_serve() {
    let down = rig_with(Some(TOML), None, |_| ScriptedHttp::new());
    let health = down.engine.health().await;
    assert!(!health.ok);
    assert_eq!(health.reason.as_deref(), Some("metrics_unreachable"));

    let no_api = without_api();
    let idle = rig(&no_api, None).engine.health().await;
    assert!(!idle.ok);
    assert_eq!(idle.reason.as_deref(), Some("no_me_writers"));
}

#[test]
fn the_engine_is_telemt_and_can_be_used_as_a_trait_object() {
    let rig = rig(TOML, None);
    let engine: Arc<dyn Engine> = Arc::new(rig.engine);
    assert_eq!(engine.kind(), EngineKind::Telemt);
    assert_eq!(engine.port(), Some(8443));
}

#[tokio::test]
async fn detect_reads_the_port_from_the_engine_config() {
    let rig = rig(TOML, None);
    assert_eq!(
        rig.engine.detect().await,
        Some(DetectedEngine {
            engine: EngineKind::Telemt,
            port: 8443
        })
    );
    let none = rig_with(None, None, |_| ScriptedHttp::new());
    assert_eq!(none.engine.detect().await, None);
    assert_eq!(none.engine.port(), None);
    let broken = rig_with(Some("[server\nport ="), None, |_| ScriptedHttp::new());
    assert_eq!(broken.engine.detect().await, None);
}

// ---------------------------------------------------------------------------------------------
// apply_config
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn apply_preserves_comments_and_foreign_users() {
    let old = owner_config();
    let rig = rig(&old, None);
    script_restart(&rig.runner);

    let outcome = rig
        .engine
        .apply_config(&desired(
            &[("alice", ALICE), ("bob", BOB)],
            "cdn.example.org",
        ))
        .await
        .unwrap();
    assert_eq!(outcome, ApplyOutcome::Applied);

    let new = rig.config_text();
    // Nothing the owner wrote is touched: every line is still there, comments and layout
    // included, and the pool's domain is a line of its own.
    for line in old.lines() {
        assert!(new.lines().any(|l| l == line), "lost: {line}");
    }
    assert!(new.contains("# telemt 3.5.16 test config for recording fixtures"));
    assert!(new.contains(&format!(
        "owner = \"{OWNER}\" # the owner's own user, not the pool's"
    )));
    let parsed = TelemtToml::parse(&new).unwrap();
    assert_eq!(parsed.port(), 8443);
    assert_eq!(parsed.tls_domain().as_deref(), Some("example.com"));
    assert_eq!(parsed.tls_domains(), ["example.com", "cdn.example.org"]);
    let users = parsed.users();
    for expected in [("alice", ALICE), ("bob", BOB), ("owner", OWNER)] {
        assert!(
            users.contains(&(expected.0.to_owned(), expected.1.to_owned())),
            "{expected:?}"
        );
    }
    // The user that was in the file before the pool came is still there.
    assert!(users.iter().any(|(name, _)| name == "probe"));

    // The old file is kept next to the new one, the pool remembers the names it wrote.
    assert_eq!(rig.file(BACKUP).as_deref(), Some(old.as_str()));
    assert_eq!(
        rig.pool_users(),
        Some(vec!["alice".to_owned(), "bob".to_owned()])
    );
    // The engine was stopped, written, started, and asked for its metrics.
    assert_eq!(
        rig.systemctl(),
        ["stop telemt.service", "start telemt.service"]
    );
    assert!(rig.http.requests().iter().any(|r| r.url == METRICS_URL));
    // Nothing is left of the temporary file.
    assert_eq!(rig.file("/etc/telemt/telemt.toml.tmp"), None);
}

#[tokio::test]
async fn apply_removes_revoked_pool_user_keeps_owner_user() {
    let config = format!("{}alice = \"{ALICE}\"\nbob = \"{BOB}\"\n", owner_config());
    let rig = rig(&config, Some(r#"["alice","bob"]"#));
    script_restart(&rig.runner);

    // The pool revoked bob; alice's secret was rotated.
    let new_alice = "dddddddddddddddddddddddddddddddd";
    let outcome = rig
        .engine
        .apply_config(&desired(&[("alice", new_alice)], "example.com"))
        .await
        .unwrap();
    assert_eq!(outcome, ApplyOutcome::Applied);

    let users = TelemtToml::parse(&rig.config_text()).unwrap().users();
    let names: Vec<&str> = users.iter().map(|(name, _)| name.as_str()).collect();
    assert!(!names.contains(&"bob"), "{names:?}");
    assert!(names.contains(&"owner"), "{names:?}");
    assert!(names.contains(&"probe"), "{names:?}");
    assert!(users.contains(&("alice".to_owned(), new_alice.to_owned())));
    assert!(users.contains(&("owner".to_owned(), OWNER.to_owned())));
    assert_eq!(rig.pool_users(), Some(vec!["alice".to_owned()]));
}

#[tokio::test]
async fn a_user_the_pool_never_wrote_is_never_removed() {
    // The state file is missing (a fresh node): the pool owns nothing yet.
    let rig = rig(&owner_config(), None);
    script_restart(&rig.runner);
    rig.engine
        .apply_config(&desired(&[("alice", ALICE)], "example.com"))
        .await
        .unwrap();
    let names: Vec<String> = TelemtToml::parse(&rig.config_text())
        .unwrap()
        .users()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(names, ["probe", "owner", "alice"]);
}

#[tokio::test]
async fn a_name_the_owner_holds_with_another_secret_is_refused() {
    let rig = rig(&owner_config(), None);
    let before = rig.config_text();
    let error = rig
        .engine
        .apply_config(&desired(
            &[("alice", ALICE), ("owner", BOB)],
            "pool.example.org",
        ))
        .await
        .unwrap_err();

    // The reason names the user and never a secret.
    assert!(
        matches!(&error, EngineError::PoolUserTaken { names } if names == &["owner"]),
        "{error}"
    );
    let said = error.to_string();
    assert!(said.contains("owner"), "{said}");
    assert!(!said.contains(BOB) && !said.contains(OWNER), "{said}");

    // Nothing of the pool went in: not the user, not the domain, and telemt was not touched.
    assert_eq!(rig.config_text(), before);
    assert_eq!(rig.pool_users(), None);
    assert!(rig.runner.calls().is_empty());

    // The engine itself is fine: a warning, not a reason for the agent to fall silent.
    let health = rig.engine.health().await;
    assert!(health.ok);
    assert_eq!(health.reason.as_deref(), Some("pool_user_taken"));
    assert!(rig.engine.metrics().await.unwrap().ok);
}

#[tokio::test]
async fn the_refusal_ends_with_the_next_config_that_takes_no_name() {
    let rig = rig(&owner_config(), None);
    rig.engine
        .apply_config(&desired(&[("owner", BOB)], "example.com"))
        .await
        .unwrap_err();

    script_restart(&rig.runner);
    let outcome = rig
        .engine
        .apply_config(&desired(&[("alice", ALICE)], "example.com"))
        .await
        .unwrap();
    assert_eq!(outcome, ApplyOutcome::Applied);
    assert_eq!(rig.engine.health().await.reason, None);
    assert_eq!(
        TelemtToml::parse(&rig.config_text()).unwrap().users(),
        [
            (
                "probe".to_owned(),
                "00112233445566778899aabbccddeeff".to_owned()
            ),
            ("owner".to_owned(), OWNER.to_owned()),
            ("alice".to_owned(), ALICE.to_owned()),
        ]
    );
}

#[tokio::test]
async fn a_name_with_the_same_secret_is_the_pools_without_a_restart() {
    // An agent that lost its state file finds what it wrote before.
    let rig = rig(&owner_config(), None);
    let outcome = rig
        .engine
        .apply_config(&desired(&[("owner", OWNER)], "example.com"))
        .await
        .unwrap();
    assert_eq!(outcome, ApplyOutcome::Unchanged);
    assert!(rig.runner.calls().is_empty());
    assert_eq!(rig.pool_users(), Some(vec!["owner".to_owned()]));
    assert_eq!(rig.engine.health().await.reason, None);
}

#[tokio::test]
async fn the_case_of_a_secret_does_not_make_it_another_one() {
    // Not refused. The file still takes the pool's spelling, as with any other change.
    let upper = OWNER.to_uppercase();
    let rig = rig(&owner_config(), None);
    script_restart(&rig.runner);
    let outcome = rig
        .engine
        .apply_config(&desired(&[("owner", &upper)], "example.com"))
        .await
        .unwrap();
    assert_eq!(outcome, ApplyOutcome::Applied);
    assert_eq!(rig.pool_users(), Some(vec!["owner".to_owned()]));
    assert_eq!(rig.engine.health().await.reason, None);
}

#[tokio::test]
async fn a_name_the_pool_wrote_before_still_rotates() {
    let rig = rig(&owner_config(), Some(r#"["owner"]"#));
    script_restart(&rig.runner);
    let outcome = rig
        .engine
        .apply_config(&desired(&[("owner", BOB)], "example.com"))
        .await
        .unwrap();
    assert_eq!(outcome, ApplyOutcome::Applied);
    assert!(rig.config_text().contains(BOB));
    assert_eq!(rig.engine.health().await.reason, None);
}

#[tokio::test]
async fn apply_noop_when_unchanged_no_restart() {
    let config = format!(
        "{}alice = \"{ALICE}\"\n",
        owner_config().replace(
            "tls_domain = \"example.com\"\n",
            "tls_domain = \"example.com\"\ntls_domains = [\"cdn.example.org\"]\n",
        )
    );
    let rig = rig(
        &config,
        Some(r#"{"users":["alice"],"sni":"cdn.example.org"}"#),
    );

    let outcome = rig
        .engine
        .apply_config(&desired(&[("alice", ALICE)], "cdn.example.org"))
        .await
        .unwrap();
    assert_eq!(outcome, ApplyOutcome::Unchanged);
    assert!(rig.runner.calls().is_empty(), "nothing is restarted");
    assert!(rig.http.requests().is_empty());
    assert_eq!(rig.config_text(), config, "the file is not rewritten");
    assert_eq!(rig.file(BACKUP), None);
    assert_eq!(rig.pool_users(), Some(vec!["alice".to_owned()]));
}

#[tokio::test]
async fn apply_unchanged_still_remembers_the_pool_users() {
    // The pool's user is already in the file (installed by hand): no restart, but from now on the
    // pool owns the name and removes it when it is revoked.
    let config = format!("{}alice = \"{ALICE}\"\n", owner_config());
    let rig = rig(&config, None);
    let outcome = rig
        .engine
        .apply_config(&desired(&[("alice", ALICE)], "example.com"))
        .await
        .unwrap();
    assert_eq!(outcome, ApplyOutcome::Unchanged);
    assert!(rig.runner.calls().is_empty());
    assert_eq!(rig.pool_users(), Some(vec!["alice".to_owned()]));
}

/// An engine that answers its metrics unless its configuration holds `marker`: the new
/// configuration is what breaks it.
struct BrokenWith {
    fs: Arc<MemFs>,
    marker: &'static str,
    requests: std::sync::atomic::AtomicUsize,
}

impl HttpGet for BrokenWith {
    async fn get(&self, _url: &str, _authorization: Option<&str>) -> Result<HttpResponse, String> {
        self.requests
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let config = String::from_utf8(self.fs.read(Path::new(CONFIG)).unwrap().unwrap()).unwrap();
        if config.contains(self.marker) {
            Err("connection refused".into())
        } else {
            ok(METRICS)
        }
    }
}

#[tokio::test(start_paused = true)]
async fn apply_rollback_when_metrics_not_ready_60s() {
    let old = owner_config();
    let rig = rig_with(Some(&old), Some(r#"["alice"]"#), |fs| BrokenWith {
        fs: fs.clone(),
        marker: "tls_domains",
        requests: 0.into(),
    });
    // stop, start (new), then the same again for the old file.
    for _ in 0..2 {
        script_restart(&rig.runner);
    }

    let started = tokio::time::Instant::now();
    let outcome = rig
        .engine
        .apply_config(&desired(&[("alice", ALICE)], "cdn.example.org"))
        .await
        .unwrap();
    let waited = started.elapsed();

    let ApplyOutcome::RolledBack(reason) = outcome else {
        panic!("expected a rollback, got {outcome:?}");
    };
    assert!(reason.contains("60"), "{reason}");
    assert!(
        waited >= Duration::from_secs(60) && waited < Duration::from_secs(70),
        "waited {waited:?}"
    );
    // Polled every two seconds.
    let polls = rig.http.requests.load(std::sync::atomic::Ordering::SeqCst);
    assert!((30..=33).contains(&polls), "{polls} polls");

    // The old file is back, byte for byte, and the engine runs on it.
    assert_eq!(rig.config_text(), old);
    assert_eq!(
        rig.systemctl(),
        [
            "stop telemt.service",
            "start telemt.service",
            "stop telemt.service",
            "start telemt.service"
        ]
    );
    // What failed is not remembered as the pool's.
    assert_eq!(rig.pool_users(), Some(vec!["alice".to_owned()]));
}

#[tokio::test(start_paused = true)]
async fn apply_waits_for_the_metrics_and_succeeds_when_they_come() {
    let rig = rig_with(Some(&owner_config()), None, |_| {
        let http = ScriptedHttp::new();
        // Not up for the first three polls.
        for _ in 0..3 {
            http.push(METRICS_URL, Err("connection refused".into()));
        }
        http.always(METRICS_URL, ok(METRICS));
        http
    });
    script_restart(&rig.runner);
    let started = tokio::time::Instant::now();
    let outcome = rig
        .engine
        .apply_config(&desired(&[("alice", ALICE)], "example.com"))
        .await
        .unwrap();
    assert_eq!(outcome, ApplyOutcome::Applied);
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_secs(6) && waited < Duration::from_secs(10),
        "{waited:?}"
    );
    assert_eq!(rig.http.requests().len(), 4);
}

#[tokio::test]
async fn apply_rolls_back_when_the_engine_does_not_start() {
    let old = owner_config();
    let rig = rig(&old, None);
    rig.runner.push_output(0, b""); // stop
    rig.runner.push_output(1, b""); // start fails
    script_restart(&rig.runner); // and back again

    let outcome = rig
        .engine
        .apply_config(&desired(&[("alice", ALICE)], "example.com"))
        .await
        .unwrap();
    let ApplyOutcome::RolledBack(reason) = outcome else {
        panic!("expected a rollback");
    };
    assert!(reason.contains("start"), "{reason}");
    assert_eq!(rig.config_text(), old);
    assert_eq!(rig.pool_users(), None);
    assert_eq!(rig.systemctl().len(), 4);
}

#[tokio::test(start_paused = true)]
async fn apply_reports_a_rollback_that_fails_too() {
    // The engine is broken whatever the file says.
    let rig = rig_with(Some(&owner_config()), None, |_| ScriptedHttp::new());
    for _ in 0..2 {
        script_restart(&rig.runner);
    }
    let error = rig
        .engine
        .apply_config(&desired(&[("alice", ALICE)], "example.com"))
        .await
        .unwrap_err();
    assert!(
        matches!(error, EngineError::RollbackFailed { .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn apply_does_not_touch_the_file_when_the_engine_cannot_be_stopped() {
    let old = owner_config();
    let rig = rig(&old, None);
    rig.runner.push_output(5, b""); // systemctl stop fails
    let error = rig
        .engine
        .apply_config(&desired(&[("alice", ALICE)], "example.com"))
        .await
        .unwrap_err();
    assert!(matches!(error, EngineError::Host(_)), "{error:?}");
    assert_eq!(rig.config_text(), old);
    assert_eq!(rig.file(BACKUP), None);
    assert_eq!(rig.pool_users(), None);
    assert_eq!(rig.runner.calls().len(), 1);
}

#[tokio::test]
async fn apply_refuses_a_configuration_without_any_user() {
    // The only user is the pool's, and the pool now wants none: telemt would not start.
    let config =
        "[server]\nport = 8443\n\n[access.users]\nalice = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n";
    let rig = rig(config, Some(r#"["alice"]"#));
    let error = rig
        .engine
        .apply_config(&desired(&[], "example.com"))
        .await
        .unwrap_err();
    assert!(matches!(error, EngineError::Config(_)), "{error:?}");
    assert!(rig.runner.calls().is_empty());
    assert_eq!(rig.config_text(), config);
}

#[tokio::test]
async fn apply_refuses_what_is_not_a_secret_or_a_domain() {
    let rig = rig(&owner_config(), None);
    for bad in [
        desired(&[("alice", "not-hex")], "example.com"),
        desired(&[("a b", ALICE)], "example.com"),
        desired(&[("", ALICE)], "example.com"),
        desired(&[("alice", ALICE)], ""),
    ] {
        let error = rig.engine.apply_config(&bad).await.unwrap_err();
        assert!(
            matches!(error, EngineError::Config(_)),
            "{bad:?}: {error:?}"
        );
    }
    assert!(rig.runner.calls().is_empty());
}

#[tokio::test]
async fn apply_without_a_config_file_is_an_error_and_starts_nothing() {
    let rig = rig_with(None, None, |_| ScriptedHttp::new());
    let error = rig
        .engine
        .apply_config(&desired(&[("alice", ALICE)], "example.com"))
        .await
        .unwrap_err();
    assert!(matches!(error, EngineError::Config(_)), "{error:?}");
    assert!(rig.runner.calls().is_empty());
}

#[tokio::test]
async fn apply_with_a_broken_state_file_keeps_nothing_it_cannot_prove() {
    // An unreadable list of the pool's names: the pool removes nothing, it does not guess.
    let config = format!("{}bob = \"{BOB}\"\n", owner_config());
    let rig = rig(&config, Some("not json"));
    script_restart(&rig.runner);
    rig.engine
        .apply_config(&desired(&[("alice", ALICE)], "example.com"))
        .await
        .unwrap();
    let names: Vec<String> = TelemtToml::parse(&rig.config_text())
        .unwrap()
        .users()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert!(names.contains(&"bob".to_owned()), "{names:?}");
    assert_eq!(rig.pool_users(), Some(vec!["alice".to_owned()]));
}

#[tokio::test(start_paused = true)]
async fn apply_is_ready_when_the_metrics_answer_even_without_me_writers() {
    // The middle proxy is on, the control API is off and no ME writer is up (its servers are
    // slow or unreachable): telemt runs on the new file, so that is not a reason to roll back.
    let config = without_api();
    // The recorded answer has no ME writer.
    let slow_me = METRICS;
    let rig = rig_with(Some(&config), None, |_| {
        let http = ScriptedHttp::new();
        http.always(METRICS_URL, ok(slow_me));
        http
    });
    script_restart(&rig.runner);
    let outcome = rig
        .engine
        .apply_config(&desired(&[("alice", ALICE)], "cdn.example.org"))
        .await
        .unwrap();
    assert_eq!(outcome, ApplyOutcome::Applied);
    assert_eq!(
        rig.systemctl(),
        ["stop telemt.service", "start telemt.service"]
    );
    assert_eq!(rig.http.requests().len(), 1, "ready at the first answer");
    // The writer rule is for health, which still says it.
    assert_eq!(
        rig.engine.health().await.reason.as_deref(),
        Some("no_me_writers")
    );
}

const WITHOUT_METRICS_KEYS: &str = concat!(
    "[server]\nport = 8443\n\n",
    "[general]\nuse_middle_proxy = true\n\n",
    "[access.users]\nprobe = \"00112233445566778899aabbccddeeff\"\n",
);

#[tokio::test]
async fn apply_adds_a_metrics_listener_when_the_file_names_none() {
    let rig = rig(WITHOUT_METRICS_KEYS, None);
    script_restart(&rig.runner);
    let outcome = rig
        .engine
        .apply_config(&desired(&[("alice", ALICE)], "example.com"))
        .await
        .unwrap();
    assert_eq!(outcome, ApplyOutcome::Applied);
    let new = rig.config_text();
    let parsed: toml_edit::DocumentMut = new.parse().unwrap();
    assert_eq!(
        parsed["server"]["metrics_listen"].as_str(),
        Some("127.0.0.1:9090")
    );
    assert!(parsed["server"].get("metrics_port").is_none());
    // The address the agent reads is the one it wrote.
    assert_eq!(metrics_url(&TelemtToml::parse(&new).unwrap()), METRICS_URL);

    // The second time there is nothing to change.
    let again = rig
        .engine
        .apply_config(&desired(&[("alice", ALICE)], "example.com"))
        .await
        .unwrap();
    assert_eq!(again, ApplyOutcome::Unchanged);
    assert_eq!(rig.config_text(), new);
    assert_eq!(rig.runner.calls().len(), 2);
}

#[tokio::test]
async fn apply_leaves_the_metrics_keys_the_owner_set() {
    for keys in [
        "metrics_port = 9100\n",
        "metrics_listen = \"127.0.0.1:9188\"\n",
    ] {
        let config = WITHOUT_METRICS_KEYS.replace("port = 8443\n", &format!("port = 8443\n{keys}"));
        let rig = rig_with(Some(&config), None, |_| {
            let http = ScriptedHttp::new();
            http.always("http://127.0.0.1:9100/metrics", ok(METRICS));
            http.always("http://127.0.0.1:9188/metrics", ok(METRICS));
            http
        });
        script_restart(&rig.runner);
        rig.engine
            .apply_config(&desired(&[("alice", ALICE)], "example.com"))
            .await
            .unwrap();
        let new = rig.config_text();
        assert!(new.contains(keys), "{new}");
        assert_eq!(new.matches("metrics_").count(), 1, "{new}");
    }
}

// ---------------------------------------------------------------------------------------------
// The owner's configuration is added to, not overwritten
// ---------------------------------------------------------------------------------------------

/// The recorded config with `extra` lines right after `tls_domain` in `[censorship]`.
fn censorship_with(extra: &str) -> String {
    let config = owner_config().replace(
        "tls_domain = \"example.com\"\n",
        &format!("tls_domain = \"example.com\"\n{extra}"),
    );
    assert!(extra.is_empty() || config != owner_config());
    config
}

/// Applies alice with `sni` to `config` (and `state`), the engine starting fine, and returns the
/// rig with the outcome.
async fn apply_alice(
    config: &str,
    state: Option<&str>,
    sni: &str,
) -> (Rig<ScriptedHttp>, ApplyOutcome) {
    let rig = rig(config, state);
    script_restart(&rig.runner);
    let outcome = rig
        .engine
        .apply_config(&desired(&[("alice", ALICE)], sni))
        .await
        .unwrap();
    (rig, outcome)
}

fn parsed(text: &str) -> toml_edit::DocumentMut {
    text.parse().unwrap()
}

fn strings(item: &toml_edit::Item) -> Vec<&str> {
    item.as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect()
}

fn keys(item: &toml_edit::Item) -> Vec<&str> {
    item.as_table_like()
        .unwrap()
        .iter()
        .map(|(k, _)| k)
        .collect()
}

#[tokio::test]
async fn owner_config_ends_with_two_users_and_two_domains() {
    let (rig, outcome) = apply_alice(&owner_config(), None, "cdn.example.org").await;
    assert_eq!(outcome, ApplyOutcome::Applied);
    let doc = parsed(&rig.config_text());
    // The owner's user, domain and port are what they were.
    assert_eq!(doc["access"]["users"]["owner"].as_str(), Some(OWNER));
    assert!(doc["access"]["users"]["probe"].as_str().is_some());
    assert_eq!(
        doc["censorship"]["tls_domain"].as_str(),
        Some("example.com")
    );
    assert_eq!(doc["server"]["port"].as_integer(), Some(8443));
    // The pool's user and domain are next to them.
    assert_eq!(doc["access"]["users"]["alice"].as_str(), Some(ALICE));
    assert_eq!(
        strings(&doc["censorship"]["tls_domains"]),
        ["cdn.example.org"]
    );
    assert_eq!(rig.pool_sni().as_deref(), Some("cdn.example.org"));
    // The owner's native link still works: the port the node reports is its own.
    assert_eq!(rig.engine.port(), Some(8443));
}

#[tokio::test]
async fn tls_domain_is_written_only_when_absent() {
    // Present: whatever it says stays.
    let (rig, _) = apply_alice(&owner_config(), None, "cdn.example.org").await;
    assert_eq!(
        parsed(&rig.config_text())["censorship"]["tls_domain"].as_str(),
        Some("example.com")
    );

    // Absent: the pool's domain is the one, and it needs no second entry.
    let without = owner_config().replace("tls_domain = \"example.com\"\n", "");
    assert_ne!(without, owner_config());
    let (rig, outcome) = apply_alice(&without, None, "cdn.example.org").await;
    assert_eq!(outcome, ApplyOutcome::Applied);
    let doc = parsed(&rig.config_text());
    assert_eq!(
        doc["censorship"]["tls_domain"].as_str(),
        Some("cdn.example.org")
    );
    assert!(doc["censorship"].get("tls_domains").is_none());

    // No `[censorship]` table at all: it is made.
    let bare = "[server]\nport = 8443\nmetrics_port = 9090\n\n[access.users]\nprobe = \"00112233445566778899aabbccddeeff\"\n";
    let (rig, _) = apply_alice(bare, None, "cdn.example.org").await;
    let doc = parsed(&rig.config_text());
    assert_eq!(
        doc["censorship"]["tls_domain"].as_str(),
        Some("cdn.example.org")
    );
    assert_eq!(doc["server"]["port"].as_integer(), Some(8443));
}

#[tokio::test]
async fn sni_equal_to_the_primary_domain_adds_nothing() {
    let (rig, outcome) = apply_alice(&owner_config(), None, "example.com").await;
    assert_eq!(outcome, ApplyOutcome::Applied);
    let doc = parsed(&rig.config_text());
    assert!(doc["censorship"].get("tls_domains").is_none());
    assert_eq!(
        doc["censorship"]["tls_domain"].as_str(),
        Some("example.com")
    );
    assert_eq!(doc["access"]["users"]["alice"].as_str(), Some(ALICE));

    // Already listed by the owner as well: still one entry.
    let listed = censorship_with("tls_domains = [\"example.com\"]\n");
    let (rig, _) = apply_alice(&listed, None, "example.com").await;
    assert_eq!(
        strings(&parsed(&rig.config_text())["censorship"]["tls_domains"]),
        ["example.com"]
    );
}

#[tokio::test]
async fn owner_extra_domains_stay_and_only_the_previous_pool_sni_goes() {
    let config = format!(
        "{}alice = \"{ALICE}\"\n",
        censorship_with(concat!(
            "tls_domains = [\n",
            "    \"one.example.net\", # the owner's\n",
            "    \"old.example.org\",\n",
            "    \"two.example.net\",\n",
            "]\n",
        ))
    );
    let state = r#"{"users":["alice"],"sni":"old.example.org"}"#;
    let (rig, outcome) = apply_alice(&config, Some(state), "new.example.org").await;
    assert_eq!(outcome, ApplyOutcome::Applied);
    let new = rig.config_text();
    let doc = parsed(&new);
    assert_eq!(
        strings(&doc["censorship"]["tls_domains"]),
        ["one.example.net", "two.example.net", "new.example.org"]
    );
    assert!(new.contains("\"one.example.net\", # the owner's"), "{new}");
    assert_eq!(rig.pool_sni().as_deref(), Some("new.example.org"));

    // The pool's domain did not change: nothing to do.
    let again = rig
        .engine
        .apply_config(&desired(&[("alice", ALICE)], "new.example.org"))
        .await
        .unwrap();
    assert_eq!(again, ApplyOutcome::Unchanged);

    // Nothing is known of an earlier domain (state of an older agent): nothing is removed.
    let (rig, _) = apply_alice(&config, Some(r#"["alice"]"#), "new.example.org").await;
    assert_eq!(
        strings(&parsed(&rig.config_text())["censorship"]["tls_domains"]),
        [
            "one.example.net",
            "old.example.org",
            "two.example.net",
            "new.example.org"
        ]
    );
}

#[tokio::test]
async fn exclusive_mask_needs_an_explicit_mask_true() {
    let table = "[censorship.exclusive_mask]\n\"owner.example.net\" = \"owner.example.net:443\"\n";
    for (mask, expected) in [
        ("", false),
        ("mask = false\n", false),
        ("mask = true\n", true),
    ] {
        let config = format!("{}{table}", censorship_with(mask));
        let (rig, _) = apply_alice(&config, None, "cdn.example.org").await;
        let doc = parsed(&rig.config_text());
        let masks = doc["censorship"]["exclusive_mask"].as_table_like().unwrap();
        assert_eq!(masks.contains_key("cdn.example.org"), expected, "{mask:?}");
        if expected {
            assert_eq!(
                masks.get("cdn.example.org").and_then(|v| v.as_str()),
                Some("cdn.example.org:443")
            );
        }
    }
    // No table yet: it is made when the mask is on, and not when it is not.
    let (rig, _) = apply_alice(&censorship_with("mask = true\n"), None, "cdn.example.org").await;
    assert_eq!(
        parsed(&rig.config_text())["censorship"]["exclusive_mask"]["cdn.example.org"].as_str(),
        Some("cdn.example.org:443")
    );
    let (rig, _) = apply_alice(&owner_config(), None, "cdn.example.org").await;
    assert!(
        parsed(&rig.config_text())["censorship"]
            .get("exclusive_mask")
            .is_none()
    );
}

#[tokio::test]
async fn owner_mask_entries_stay() {
    let config = format!(
        "{}[censorship.exclusive_mask]\n# the owner's\n\"owner.example.net\" = \"owner.example.net:8443\"\n",
        censorship_with("mask = true\n")
    );
    let (rig, _) = apply_alice(&config, None, "cdn.example.org").await;
    let new = rig.config_text();
    assert!(
        new.contains("# the owner's\n\"owner.example.net\" = \"owner.example.net:8443\"\n"),
        "{new}"
    );
    let doc = parsed(&new);
    assert_eq!(
        doc["censorship"]["exclusive_mask"]["cdn.example.org"].as_str(),
        Some("cdn.example.org:443")
    );
    // An entry of the owner's under the pool's own name is not replaced either.
    let theirs = format!(
        "{}[censorship.exclusive_mask]\n\"cdn.example.org\" = \"elsewhere.example.net:443\"\n",
        censorship_with("mask = true\n")
    );
    let (rig, _) = apply_alice(&theirs, None, "cdn.example.org").await;
    assert_eq!(
        parsed(&rig.config_text())["censorship"]["exclusive_mask"]["cdn.example.org"].as_str(),
        Some("elsewhere.example.net:443")
    );
}

#[tokio::test]
async fn previous_pool_mask_entry_goes_on_sni_change() {
    let table = concat!(
        "[censorship.exclusive_mask]\n",
        "\"owner.example.net\" = \"owner.example.net:443\"\n",
        "\"old.example.org\" = \"old.example.org:443\"\n",
    );
    let state = r#"{"users":["alice"],"sni":"old.example.org"}"#;
    let with_alice = |mask: &str| format!("{}alice = \"{ALICE}\"\n{table}", censorship_with(mask));
    // The mask is on: the old entry is swapped for the new one, the owner's stays.
    let (rig, _) = apply_alice(&with_alice("mask = true\n"), Some(state), "new.example.org").await;
    let doc = parsed(&rig.config_text());
    assert_eq!(
        keys(&doc["censorship"]["exclusive_mask"]),
        ["owner.example.net", "new.example.org"]
    );

    // The mask has been turned off since: the old entry still goes, no new one comes.
    let (rig, _) = apply_alice(
        &with_alice("mask = false\n"),
        Some(state),
        "new.example.org",
    )
    .await;
    let doc = parsed(&rig.config_text());
    assert_eq!(
        keys(&doc["censorship"]["exclusive_mask"]),
        ["owner.example.net"]
    );

    // An entry of that name that is not the pool's shape is the owner's and stays.
    let theirs = with_alice("mask = true\n").replace("old.example.org:443", "else.example.net:443");
    let (rig, _) = apply_alice(&theirs, Some(state), "new.example.org").await;
    let doc = parsed(&rig.config_text());
    assert_eq!(
        doc["censorship"]["exclusive_mask"]["old.example.org"].as_str(),
        Some("else.example.net:443")
    );
}

#[tokio::test]
async fn engine_port_is_never_written() {
    // The port differs from the pool's in `[server]`.
    let (rig, _) = apply_alice(&owner_config(), None, "cdn.example.org").await;
    let doc = parsed(&rig.config_text());
    assert_eq!(doc["server"]["port"].as_integer(), Some(8443));

    // In a listener, which keeps its own port; the one without keeps having none.
    let config = concat!(
        // No middle proxy: no writers to wait for, so the recorded metrics count as serving.
        "[general]\nuse_middle_proxy = false\n\n",
        "[server]\nmetrics_port = 9090\n\n",
        "[[server.listeners]]\nip = \"0.0.0.0\"\nport = 8443\n\n",
        "[[server.listeners]]\nip = \"::\"\n",
    );
    let (rig, outcome) = apply_alice(config, None, "cdn.example.org").await;
    assert_eq!(outcome, ApplyOutcome::Applied);
    let new = rig.config_text();
    let doc = parsed(&new);
    assert!(doc["server"].get("port").is_none(), "{new}");
    let listeners = doc["server"]["listeners"].as_array_of_tables().unwrap();
    assert_eq!(listeners.get(0).unwrap()["port"].as_integer(), Some(8443));
    assert!(listeners.get(1).unwrap().get("port").is_none());
    // The missing tables are made, the pool's user is in.
    assert_eq!(doc["access"]["users"]["alice"].as_str(), Some(ALICE));
    assert_eq!(
        doc["censorship"]["tls_domain"].as_str(),
        Some("cdn.example.org")
    );

    // Absent everywhere: telemt's default stays the default.
    let bare = "[server]\nmetrics_port = 9090\n";
    let (rig, _) = apply_alice(bare, None, "cdn.example.org").await;
    assert!(parsed(&rig.config_text())["server"].get("port").is_none());
}

#[tokio::test]
async fn second_apply_changes_nothing_and_does_not_restart() {
    let config = censorship_with("mask = true\n");
    let (rig, first) = apply_alice(&config, None, "cdn.example.org").await;
    assert_eq!(first, ApplyOutcome::Applied);
    let written = rig.config_text();
    let calls = rig.runner.calls().len();
    let requests = rig.http.requests().len();

    let second = rig
        .engine
        .apply_config(&desired(&[("alice", ALICE)], "cdn.example.org"))
        .await
        .unwrap();
    assert_eq!(second, ApplyOutcome::Unchanged);
    assert_eq!(rig.config_text(), written, "byte for byte");
    assert_eq!(rig.runner.calls().len(), calls, "no restart");
    assert_eq!(rig.http.requests().len(), requests);
}

#[tokio::test]
async fn state_file_of_an_older_agent_still_loads() {
    // The old file is a bare list of names, with no domain in it.
    let config = format!("{}alice = \"{ALICE}\"\nbob = \"{BOB}\"\n", owner_config());
    let (rig, outcome) = apply_alice(&config, Some(r#"["alice","bob"]"#), "cdn.example.org").await;
    assert_eq!(outcome, ApplyOutcome::Applied);
    let names: Vec<String> = TelemtToml::parse(&rig.config_text())
        .unwrap()
        .users()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    // The names were read: bob, the pool's and revoked, is gone, the owner's stay.
    assert!(!names.contains(&"bob".to_owned()), "{names:?}");
    assert!(names.contains(&"owner".to_owned()), "{names:?}");
    // From now on the file has both.
    assert_eq!(rig.pool_users(), Some(vec!["alice".to_owned()]));
    assert_eq!(rig.pool_sni().as_deref(), Some("cdn.example.org"));
}

#[tokio::test]
async fn tls_domains_of_a_wrong_type_is_an_error_and_writes_nothing() {
    for wrong in [
        "tls_domains = \"one.example.net\"\n",
        "tls_domains = [\"a\", 1]\n",
    ] {
        let config = censorship_with(wrong);
        let rig = rig(&config, None);
        let error = rig
            .engine
            .apply_config(&desired(&[("alice", ALICE)], "cdn.example.org"))
            .await
            .unwrap_err();
        assert!(
            matches!(error, EngineError::Config(_)),
            "{wrong}: {error:?}"
        );
        assert!(rig.runner.calls().is_empty());
        assert_eq!(rig.config_text(), config);
        assert_eq!(rig.file(BACKUP), None);
        assert_eq!(rig.pool_users(), None);
    }
}

fn agent_config(users: &[(&str, &str)], sni: &str, port: u16) -> AgentConfig {
    AgentConfig {
        users: users
            .iter()
            .map(|(name, secret)| AgentUser {
                name: (*name).to_owned(),
                secret_hex: (*secret).to_owned(),
            })
            .collect(),
        sni: sni.to_owned(),
        port,
        intervals: Intervals {
            heartbeat_sec: 15,
            globalping_sec: 300,
            metrics_sec: 60,
            sync_sec: 60,
        },
        antiscan: false,
        report_other_users: false,
    }
}

#[tokio::test]
async fn the_config_sink_applies_what_the_registrar_sent() {
    let rig = rig(&owner_config(), None);
    script_restart(&rig.runner);
    rig.engine
        .apply(&agent_config(&[("alice", ALICE)], "cdn.example.org", 9443))
        .await;
    let parsed = TelemtToml::parse(&rig.config_text()).unwrap();
    // The port is the owner's; the pool's domain and user are added.
    assert_eq!(parsed.port(), 8443);
    assert_eq!(parsed.tls_domains(), ["example.com", "cdn.example.org"]);
    assert!(parsed.users().iter().any(|(name, _)| name == "alice"));
    // A failure is logged, not raised: the lifecycle goes on and asks again next round.
    let broken = rig_with(None, None, |_| ScriptedHttp::new());
    broken
        .engine
        .apply(&agent_config(&[("alice", ALICE)], "example.com", 8443))
        .await;
    assert!(broken.runner.calls().is_empty());
}

#[test]
fn desired_config_comes_from_the_agent_config() {
    let config = agent_config(&[("alice", ALICE), ("bob", BOB)], "cdn.example.org", 9443);
    assert_eq!(
        DesiredConfig::from(&config),
        desired(&[("alice", ALICE), ("bob", BOB)], "cdn.example.org")
    );
}
