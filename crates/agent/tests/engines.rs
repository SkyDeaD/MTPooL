//! The engine detection and the MTProxyL and MEKO adapters. MTProxyL is driven through its CLI,
//! MEKO runs the telemt adapter, and both read telemt's own answers: the recorded telemt 3.5.16
//! metrics in `fixtures/telemt`. The MTProxyL files in `fixtures/mtproxyl` are DERIVED from the
//! documented layout, not recorded (MTProxyL is not installed anywhere here). Nothing runs a
//! command or touches the real file system: all of it goes through scripted hosts.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use mtpool_agent::engine::detect::{SERVICE_UNIT_CANDIDATES, detect_engine, find_service_unit};
use mtpool_agent::engine::http::testing::ScriptedHttp;
use mtpool_agent::engine::http::{HttpGet, HttpResponse};
use mtpool_agent::engine::meko::{self, MekoEngine};
use mtpool_agent::engine::mtproxyl::{self, MtProxyLEngine};
use mtpool_agent::engine::{ApplyOutcome, DesiredConfig, Engine, EngineError, EngineKind};
use mtpool_agent::host::testing::ScriptedRunner;
use mtpool_agent::lifecycle::{ConfigSink, DetectedEngine, EngineProbe};
use mtpool_agent::state_files::{Fs, MemFs};
use mtpool_protocol::agent::{AgentConfig, AgentUser, Intervals};

const METRICS: &str = include_str!("fixtures/telemt/metrics.txt");
const DCS: &str = include_str!("fixtures/telemt/dcs.json");
const METRICS_USERS: &str = include_str!("fixtures/mtproxyl/metrics_users.txt");
const CONFIG_TOML: &str = include_str!("fixtures/mtproxyl/config.toml");
const SETTINGS: &str = include_str!("fixtures/mtproxyl/settings.conf");

const CONFIG: &str = "/opt/mtproxyl/mtproxy/config.toml";
const SETTINGS_PATH: &str = "/opt/mtproxyl/settings.conf";
const STATE_DIR: &str = "/state";
const POOL_USERS: &str = "/state/pool_users.json";
const METRICS_URL: &str = "http://127.0.0.1:9090/metrics";
const DCS_URL: &str = "http://127.0.0.1:9091/v1/stats/dcs";
const MEKO_CONFIG: &str = "/etc/telemt/telemt.toml";
const DCS_AT: u64 = 1_793_732_062;

const ALICE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const ALICE_NEW: &str = "dddddddddddddddddddddddddddddddd";
const BOB: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const OWNER: &str = "cccccccccccccccccccccccccccccccc";

fn ok(body: &str) -> Result<HttpResponse, String> {
    Ok(HttpResponse {
        status: 200,
        body: body.to_owned(),
    })
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

fn put(fs: &MemFs, path: &str, text: &str) {
    fs.write_private(Path::new(path), text.as_bytes()).unwrap();
}

fn text_of(fs: &MemFs, path: &str) -> Option<String> {
    fs.read(Path::new(path))
        .unwrap()
        .map(|bytes| String::from_utf8(bytes).unwrap())
}

// ---------------------------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------------------------

#[test]
fn detect_mtpr_simple_meko() {
    let fs = MemFs::new();
    put(&fs, "/opt/mtpr-simple/bin/run", "x");
    assert_eq!(detect_engine(&fs), EngineKind::Meko);

    // MEKO wins when both managers' directories are there, as on the hosts it was written for.
    put(&fs, "/opt/mtproxyl/settings.conf", SETTINGS);
    assert_eq!(detect_engine(&fs), EngineKind::Meko);
}

#[test]
fn detect_opt_mtproxyl() {
    let fs = MemFs::new();
    put(&fs, SETTINGS_PATH, SETTINGS);
    assert_eq!(detect_engine(&fs), EngineKind::MtProxyL);
}

#[test]
fn detect_default_telemt() {
    let fs = MemFs::new();
    assert_eq!(detect_engine(&fs), EngineKind::Telemt);
    put(&fs, "/etc/telemt/telemt.toml", "x");
    assert_eq!(detect_engine(&fs), EngineKind::Telemt);
}

#[test]
fn detect_looks_at_whole_directory_names() {
    // `/opt/mtproxyl-old` is not `/opt/mtproxyl`, and a file of that name is not a directory of it.
    let fs = MemFs::new();
    put(&fs, "/opt/mtproxyl-old/settings.conf", "x");
    put(&fs, "/opt/mtpr-simple.bak", "x");
    assert_eq!(detect_engine(&fs), EngineKind::Telemt);
}

#[tokio::test]
async fn service_unit_candidates_in_order() {
    assert_eq!(
        SERVICE_UNIT_CANDIDATES,
        [
            "telemt.service",
            "mtproxy.service",
            "mtproxyl-telemt.service"
        ]
    );

    // The first that `systemctl cat` finds wins; the later ones are not asked.
    let runner = ScriptedRunner::new();
    runner.push_output(1, b"");
    runner.push_output(0, b"[Unit]\n");
    assert_eq!(
        find_service_unit(&runner).await.as_deref(),
        Some("mtproxy.service")
    );
    let asked: Vec<_> = runner
        .calls()
        .iter()
        .map(|c| format!("{} {}", c.program, c.args.join(" ")))
        .collect();
    assert_eq!(
        asked,
        [
            "systemctl cat telemt.service",
            "systemctl cat mtproxy.service"
        ]
    );

    let runner = ScriptedRunner::new();
    runner.push_output(0, b"");
    assert_eq!(
        find_service_unit(&runner).await.as_deref(),
        Some("telemt.service")
    );
    assert_eq!(runner.calls().len(), 1);

    // A unit that cannot be asked about (no systemctl) is a unit that is not found.
    let runner = ScriptedRunner::new();
    runner.push_error(io::Error::other("no systemctl"));
    runner.push_output(1, b"");
    runner.push_output(0, b"");
    assert_eq!(
        find_service_unit(&runner).await.as_deref(),
        Some("mtproxyl-telemt.service")
    );

    let runner = ScriptedRunner::new();
    for _ in 0..3 {
        runner.push_output(1, b"");
    }
    assert_eq!(find_service_unit(&runner).await, None);
    assert_eq!(runner.calls().len(), 3);
}

// ---------------------------------------------------------------------------------------------
// MTProxyL
// ---------------------------------------------------------------------------------------------

struct Rig<H: HttpGet + 'static> {
    fs: Arc<MemFs>,
    runner: Arc<ScriptedRunner>,
    engine: MtProxyLEngine<ScriptedRunner, H>,
}

impl<H: HttpGet + 'static> Rig<H> {
    /// The calls, each as the CLI arguments after `env MTPROXYL_ASSUME_YES=1 mtproxyl`. Every
    /// call must have that prefix: nothing else is run, `systemctl` least of all.
    fn cli(&self) -> Vec<String> {
        self.runner
            .calls()
            .iter()
            .map(|call| {
                assert_eq!(call.program, "env", "{call:?}");
                assert_eq!(call.args[0], "MTPROXYL_ASSUME_YES=1", "{call:?}");
                assert_eq!(call.args[1], "mtproxyl", "{call:?}");
                call.args[2..].join(" ")
            })
            .collect()
    }
    /// The names the pool remembers: a bare list (an older agent) or an object with `users`.
    fn pool_users(&self) -> Option<Vec<String>> {
        let value: serde_json::Value =
            serde_json::from_str(&text_of(&self.fs, POOL_USERS)?).unwrap();
        let list = if value.is_array() {
            value
        } else {
            value["users"].clone()
        };
        Some(serde_json::from_value(list).unwrap())
    }
    /// The TLS domain the pool remembers it added.
    fn pool_sni(&self) -> Option<String> {
        let value: serde_json::Value =
            serde_json::from_str(&text_of(&self.fs, POOL_USERS)?).unwrap();
        value["sni"].as_str().map(str::to_owned)
    }
    fn settings(&self) -> String {
        text_of(&self.fs, SETTINGS_PATH).unwrap()
    }
}

fn rig_with<H: HttpGet + 'static>(
    config: &str,
    settings: &str,
    pool_users: Option<&str>,
    http: impl FnOnce(&Arc<ScriptedRunner>) -> H,
) -> Rig<H> {
    let fs = Arc::new(MemFs::new());
    put(&fs, CONFIG, config);
    put(&fs, SETTINGS_PATH, settings);
    if let Some(users) = pool_users {
        put(&fs, POOL_USERS, users);
    }
    let runner = Arc::new(ScriptedRunner::new());
    let http = Arc::new(http(&runner));
    let engine = MtProxyLEngine::new(STATE_DIR.into(), runner.clone(), fs.clone(), http)
        .with_clock(|| DCS_AT + 10);
    Rig { fs, runner, engine }
}

fn rig(config: &str, pool_users: Option<&str>) -> Rig<ScriptedHttp> {
    rig_with(config, SETTINGS, pool_users, |_| {
        let http = ScriptedHttp::new();
        http.always(METRICS_URL, ok(METRICS));
        http.always(DCS_URL, ok(DCS));
        http
    })
}

fn push_ok(runner: &ScriptedRunner, count: usize) {
    for _ in 0..count {
        runner.push_output(0, b"");
    }
}

/// The new engine answers its metrics only after the CLI restarted it once more than `after`
/// times: until then the answer is a refusal.
struct DownUntilRestart {
    runner: Arc<ScriptedRunner>,
    restarts_before_up: usize,
}

impl HttpGet for DownUntilRestart {
    async fn get(&self, _url: &str, _authorization: Option<&str>) -> Result<HttpResponse, String> {
        let restarts = self
            .runner
            .calls()
            .iter()
            .filter(|call| call.args.last().is_some_and(|a| a == "restart"))
            .count();
        if restarts > self.restarts_before_up {
            ok(METRICS)
        } else {
            Err("connection refused".to_owned())
        }
    }
}

#[tokio::test]
async fn mtproxyl_apply_uses_cli_sequence() {
    let rig = rig(CONFIG_TOML, Some(r#"["alice"]"#));
    push_ok(&rig.runner, 5);

    let outcome = rig
        .engine
        .apply_config(&desired(
            &[("alice", ALICE), ("bob", BOB)],
            "cdn.example.org",
        ))
        .await
        .unwrap();

    assert_eq!(outcome, ApplyOutcome::Applied);
    // alice is already right: no `secret add` for her. Then the domain, the mask, `expert apply`
    // and the restart through the CLI.
    assert_eq!(
        rig.cli(),
        [
            format!("secret add bob {BOB}"),
            "expert set censorship tls_domains cdn.example.org --no-apply".to_owned(),
            // The owner's pair stays, the pool's follows it.
            "expert set --raw censorship.exclusive_mask example.com example.com:443 cdn.example.org cdn.example.org:443 --no-apply"
                .to_owned(),
            "expert apply".to_owned(),
            "restart".to_owned(),
        ]
    );
    assert_eq!(
        rig.pool_users(),
        Some(vec!["alice".to_owned(), "bob".to_owned()])
    );
}

#[tokio::test]
async fn mtproxyl_never_uses_plain_systemctl_restart() {
    // `cli()` fails on anything that is not `env MTPROXYL_ASSUME_YES=1 mtproxyl ...`, so a
    // `systemctl` call anywhere, in a good apply or in a rolled back one, fails the test.
    let good = rig(CONFIG_TOML, None);
    push_ok(&good.runner, 5);
    good.engine
        .apply_config(&desired(&[("bob", BOB)], "cdn.example.org"))
        .await
        .unwrap();
    for call in good.runner.calls() {
        assert_ne!(call.program, "systemctl");
        assert!(!call.args.iter().any(|a| a.contains("systemctl")));
    }
    assert_eq!(good.cli().last().map(String::as_str), Some("restart"));

    let bad = rig(CONFIG_TOML, None);
    for code in [0, 0, 0, 0, 1, 0, 0, 0, 0, 0] {
        bad.runner.push_output(code, b"");
    }
    bad.engine
        .apply_config(&desired(&[("bob", BOB)], "cdn.example.org"))
        .await
        .unwrap();
    let all = bad.cli();
    assert!(all.iter().all(|c| !c.contains("systemctl")));
    assert_eq!(all.iter().filter(|c| *c == "restart").count(), 2);
}

#[tokio::test(start_paused = true)]
async fn mtproxyl_rollback_on_failed_restart() {
    let rig = rig(CONFIG_TOML, Some(r#"["alice"]"#));
    // Five steps forward, the restart (the fifth) fails; then the undo: two commands, the apply
    // and the restart.
    for code in [0, 0, 0, 0, 1, 0, 0, 0, 0] {
        rig.runner.push_output(code, b"");
    }

    let outcome = rig
        .engine
        .apply_config(&desired(
            &[("alice", ALICE), ("bob", BOB)],
            "cdn.example.org",
        ))
        .await
        .unwrap();

    let ApplyOutcome::RolledBack(reason) = outcome else {
        panic!("expected a rollback, got {outcome:?}");
    };
    assert!(reason.contains("restart"), "{reason}");
    assert!(!reason.contains(BOB), "a secret is not repeated: {reason}");
    assert_eq!(
        rig.cli(),
        [
            format!("secret add bob {BOB}"),
            "expert set censorship tls_domains cdn.example.org --no-apply".to_owned(),
            // The owner's pair stays, the pool's follows it.
            "expert set --raw censorship.exclusive_mask example.com example.com:443 cdn.example.org cdn.example.org:443 --no-apply"
                .to_owned(),
            "expert apply".to_owned(),
            "restart".to_owned(),
            // The same way back: the mask as it was, bob removed. There were no extras, so the
            // domain stays: nothing recorded says the CLI takes an empty list.
            "expert set --raw censorship.exclusive_mask example.com example.com:443 --no-apply"
                .to_owned(),
            "secret remove bob".to_owned(),
            "expert apply".to_owned(),
            "restart".to_owned(),
        ]
    );
    // What failed is not remembered as the pool's.
    assert_eq!(rig.pool_users(), Some(vec!["alice".to_owned()]));
}

#[tokio::test(start_paused = true)]
async fn mtproxyl_rolls_back_when_the_metrics_do_not_answer() {
    let rig = rig_with(CONFIG_TOML, SETTINGS, None, |runner| DownUntilRestart {
        runner: runner.clone(),
        restarts_before_up: 1,
    });
    // Forward: add, domain, mask, apply, restart. Back: remove, then domain and mask, apply,
    // restart.
    push_ok(&rig.runner, 10);

    let started = tokio::time::Instant::now();
    let outcome = rig
        .engine
        .apply_config(&desired(&[("bob", BOB)], "cdn.example.org"))
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
    let all = rig.cli();
    assert_eq!(all.len(), 9);
    assert_eq!(&all[3..5], ["expert apply", "restart"]);
    assert_eq!(&all[7..], ["expert apply", "restart"]);
}

#[tokio::test(start_paused = true)]
async fn mtproxyl_reports_a_rollback_that_fails_too() {
    let rig = rig(CONFIG_TOML, None);
    // The restart fails, the undo's commands go through, the restart after them fails again.
    for code in [0, 0, 0, 0, 1, 0, 0, 0, 1] {
        rig.runner.push_output(code, b"");
    }
    let error = rig
        .engine
        .apply_config(&desired(&[("bob", BOB)], "cdn.example.org"))
        .await
        .unwrap_err();
    assert!(
        matches!(error, EngineError::RollbackFailed { .. }),
        "{error}"
    );
    assert!(!error.to_string().contains(BOB), "{error}");
    assert_eq!(rig.pool_users(), None);
}

#[tokio::test(start_paused = true)]
async fn mtproxyl_a_failed_step_undoes_what_was_done_and_never_prints_a_secret() {
    let rig = rig(CONFIG_TOML, Some(r#"["alice"]"#));
    // `secret add` of the second user fails. The undo is best effort: removing the half-added
    // one may fail too, and the rest still goes through.
    for code in [0, 3, 1, 0, 0, 0] {
        rig.runner.push_output(code, b"");
    }
    let outcome = rig
        .engine
        .apply_config(&desired(
            &[("alice", ALICE_NEW), ("bob", BOB)],
            "example.com",
        ))
        .await
        .unwrap();

    let ApplyOutcome::RolledBack(reason) = outcome else {
        panic!("expected a rollback, got {outcome:?}");
    };
    // alice's secret differs: she is removed and added again, then bob; the add of alice fails.
    assert!(reason.contains("secret add alice"), "{reason}");
    assert!(
        !reason.contains(ALICE_NEW) && !reason.contains(BOB),
        "{reason}"
    );
    assert_eq!(
        rig.cli(),
        [
            "secret remove alice".to_owned(),
            format!("secret add alice {ALICE_NEW}"),
            // Undo, newest first: what the failed add may have done, then the removal.
            "secret remove alice".to_owned(),
            format!("secret add alice {ALICE}"),
            "expert apply".to_owned(),
            "restart".to_owned(),
        ]
    );
}

#[tokio::test]
async fn mtproxyl_removes_revoked_pool_users_only() {
    // The pool wrote alice before; the owner's own user is in the file too and is not the pool's.
    let rig = rig(CONFIG_TOML, Some(r#"["alice"]"#));
    push_ok(&rig.runner, 4);
    let outcome = rig
        .engine
        .apply_config(&desired(&[("bob", BOB)], "example.com"))
        .await
        .unwrap();
    assert_eq!(outcome, ApplyOutcome::Applied);
    assert_eq!(
        rig.cli(),
        [
            "secret remove alice".to_owned(),
            format!("secret add bob {BOB}"),
            "expert apply".to_owned(),
            "restart".to_owned(),
        ]
    );
    assert_eq!(rig.pool_users(), Some(vec!["bob".to_owned()]));
}

#[tokio::test]
async fn mtproxyl_refuses_a_name_the_owner_holds_with_another_secret() {
    let rig = rig(CONFIG_TOML, None);
    let error = rig
        .engine
        .apply_config(&desired(&[("owner", BOB)], "pool.example.org"))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, EngineError::PoolUserTaken { names } if names == &["owner"]),
        "{error}"
    );
    assert!(rig.cli().is_empty());
    assert_eq!(rig.pool_users(), None);
    let health = rig.engine.health().await;
    assert!(health.ok);
    assert_eq!(health.reason.as_deref(), Some("pool_user_taken"));
}

#[tokio::test]
async fn mtproxyl_rotates_a_changed_secret() {
    let rig = rig(CONFIG_TOML, Some(r#"["alice"]"#));
    push_ok(&rig.runner, 4);
    rig.engine
        .apply_config(&desired(&[("alice", ALICE_NEW)], "example.com"))
        .await
        .unwrap();
    assert_eq!(
        rig.cli(),
        [
            "secret remove alice".to_owned(),
            format!("secret add alice {ALICE_NEW}"),
            "expert apply".to_owned(),
            "restart".to_owned(),
        ]
    );
}

#[tokio::test]
async fn mtproxyl_unchanged_touches_nothing() {
    // Even with superexpert on: nothing to change means nothing is touched.
    let settings = SETTINGS.replace(
        "SUPEREXPERT_ENABLED=\"false\"",
        "SUPEREXPERT_ENABLED=\"true\"",
    );
    let rig = rig_with(CONFIG_TOML, &settings, Some(r#"[]"#), |_| {
        let http = ScriptedHttp::new();
        http.always(METRICS_URL, ok(METRICS));
        http
    });
    let outcome = rig
        .engine
        .apply_config(&desired(&[("alice", ALICE)], "example.com"))
        .await
        .unwrap();
    assert_eq!(outcome, ApplyOutcome::Unchanged);
    assert!(rig.runner.calls().is_empty());
    assert_eq!(rig.settings(), settings);
    // A user that was in the file already is the pool's from now on.
    assert_eq!(rig.pool_users(), Some(vec!["alice".to_owned()]));
}

#[tokio::test]
async fn mtproxyl_a_domain_in_the_tls_domains_list_counts() {
    let config = CONFIG_TOML
        .replace(
            "tls_domain = \"example.com\"",
            "tls_domain = \"example.com\"\ntls_domains = [\"cdn.example.org\"]",
        )
        .replace(
            "\"example.com\" = \"example.com:443\"",
            "\"example.com\" = \"example.com:443\"\n\"cdn.example.org\" = \"cdn.example.org:443\"",
        );
    let rig = rig(&config, None);
    let outcome = rig
        .engine
        .apply_config(&desired(&[("alice", ALICE)], "cdn.example.org"))
        .await
        .unwrap();
    assert_eq!(outcome, ApplyOutcome::Unchanged);
    assert!(rig.runner.calls().is_empty());
}

/// The fixture with the owner's extra domains and mask pairs added to what it has.
fn with_owner_extras(extras: &[&str], pairs: &[&str]) -> String {
    let list = extras
        .iter()
        .map(|d| format!("\"{d}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let masks: String = pairs
        .iter()
        .map(|d| format!("\n\"{d}\" = \"{d}:443\""))
        .collect();
    let mut config = CONFIG_TOML.to_owned();
    if !extras.is_empty() {
        config = config.replace(
            "tls_domain = \"example.com\"",
            &format!("tls_domain = \"example.com\"\ntls_domains = [{list}]"),
        );
    }
    config.replace(
        "\"example.com\" = \"example.com:443\"",
        &format!("\"example.com\" = \"example.com:443\"{masks}"),
    )
}

#[tokio::test]
async fn keeps_owner_extra_domains() {
    // The owner's extra domain stays, the pool's previous one goes, the new one is added; the
    // primary `tls_domain` is not among them. The owner's mask pairs stay as well.
    let config = with_owner_extras(
        &["owner.example", "old.example"],
        &["owner.example", "old.example"],
    );
    let rig = rig(&config, Some(r#"{"users":["alice"],"sni":"old.example"}"#));
    push_ok(&rig.runner, 4);
    let outcome = rig
        .engine
        .apply_config(&desired(&[("alice", ALICE)], "cdn.example.org"))
        .await
        .unwrap();
    assert_eq!(outcome, ApplyOutcome::Applied);
    assert_eq!(
        rig.cli(),
        [
            "expert set censorship tls_domains owner.example cdn.example.org --no-apply",
            "expert set --raw censorship.exclusive_mask example.com example.com:443 owner.example owner.example:443 cdn.example.org cdn.example.org:443 --no-apply",
            "expert apply",
            "restart",
        ]
    );
    assert_eq!(rig.pool_sni().as_deref(), Some("cdn.example.org"));
}

#[tokio::test]
async fn undo_restores_the_extras() {
    let config = with_owner_extras(&["owner.example"], &["owner.example"]);
    let rig = rig(&config, Some(r#"{"users":["alice"]}"#));
    // The domains, the mask, the apply, then the restart fails; the undo runs the two settings
    // back, the apply and the restart.
    for code in [0, 0, 0, 1, 0, 0, 0, 0] {
        rig.runner.push_output(code, b"");
    }
    let outcome = rig
        .engine
        .apply_config(&desired(&[("alice", ALICE)], "cdn.example.org"))
        .await
        .unwrap();
    assert!(
        matches!(outcome, ApplyOutcome::RolledBack(_)),
        "{outcome:?}"
    );
    assert_eq!(
        rig.cli(),
        [
            "expert set censorship tls_domains owner.example cdn.example.org --no-apply",
            "expert set --raw censorship.exclusive_mask example.com example.com:443 owner.example owner.example:443 cdn.example.org cdn.example.org:443 --no-apply",
            "expert apply",
            "restart",
            // The mask and the extras as they were, the owner's included.
            "expert set --raw censorship.exclusive_mask example.com example.com:443 owner.example owner.example:443 --no-apply",
            "expert set censorship tls_domains owner.example --no-apply",
            "expert apply",
            "restart",
        ]
    );
    // What failed is not remembered.
    assert_eq!(rig.pool_sni(), None);
}

#[tokio::test]
async fn mask_carries_owner_pairs() {
    let config = with_owner_extras(&[], &["owner.example"]);
    let rig = rig(&config, None);
    push_ok(&rig.runner, 4);
    rig.engine
        .apply_config(&desired(&[("alice", ALICE)], "cdn.example.org"))
        .await
        .unwrap();
    let cli = rig.cli();
    assert_eq!(
        cli[1],
        "expert set --raw censorship.exclusive_mask example.com example.com:443 owner.example owner.example:443 cdn.example.org cdn.example.org:443 --no-apply"
    );
}

#[tokio::test]
async fn no_steps_when_already_right() {
    let config = with_owner_extras(&["cdn.example.org"], &["cdn.example.org"]);
    let rig = rig(
        &config,
        Some(r#"{"users":["alice"],"sni":"cdn.example.org"}"#),
    );
    for _ in 0..2 {
        let outcome = rig
            .engine
            .apply_config(&desired(&[("alice", ALICE)], "cdn.example.org"))
            .await
            .unwrap();
        assert_eq!(outcome, ApplyOutcome::Unchanged);
    }
    assert!(rig.runner.calls().is_empty());
    assert_eq!(rig.pool_sni().as_deref(), Some("cdn.example.org"));
}

#[tokio::test]
async fn mtproxyl_superexpert_off_when_enabled() {
    // The quoted `true` of the references.
    let on = SETTINGS.replace(
        "SUPEREXPERT_ENABLED=\"false\"",
        "SUPEREXPERT_ENABLED=\"true\"",
    );
    let rig = rig_with(CONFIG_TOML, &on, None, |_| {
        let http = ScriptedHttp::new();
        http.always(METRICS_URL, ok(METRICS));
        http
    });
    push_ok(&rig.runner, 6);
    rig.engine
        .apply_config(&desired(&[("bob", BOB)], "example.com"))
        .await
        .unwrap();
    // First thing, before any secret: the CLI is asked to switch it off.
    assert_eq!(rig.cli()[0], "superexpert off");
    // The file says it is off, other lines are as they were.
    assert_eq!(rig.settings(), SETTINGS);

    // The `1` form: switched off to `0`.
    let on = "AD_TAG=\"\"\nSUPEREXPERT_ENABLED=1\n";
    let rig = rig_with(CONFIG_TOML, on, None, |_| {
        let http = ScriptedHttp::new();
        http.always(METRICS_URL, ok(METRICS));
        http
    });
    push_ok(&rig.runner, 6);
    rig.engine
        .apply_config(&desired(&[("bob", BOB)], "example.com"))
        .await
        .unwrap();
    assert_eq!(rig.cli()[0], "superexpert off");
    assert_eq!(rig.settings(), "AD_TAG=\"\"\nSUPEREXPERT_ENABLED=0\n");

    // Off already, or no settings file: the CLI is not asked.
    let rig = self::rig(CONFIG_TOML, None);
    push_ok(&rig.runner, 4);
    rig.engine
        .apply_config(&desired(&[("bob", BOB)], "example.com"))
        .await
        .unwrap();
    assert!(!rig.cli().iter().any(|c| c.starts_with("superexpert")));
    assert_eq!(rig.settings(), SETTINGS);
}

#[tokio::test]
async fn mtproxyl_refuses_what_it_cannot_apply_before_running_anything() {
    let rig = rig(CONFIG_TOML, Some(r#"["alice"]"#));
    for (users, sni) in [
        (vec![("bob", "not-a-secret")], "example.com"),
        (vec![("bob", BOB)], "bad domain"),
        (vec![("bad name", BOB)], "example.com"),
        // alice is the pool's and revoked, the owner's user is not: one user is left, so that is
        // fine; but with the owner's user gone from the file there would be none.
    ] {
        let error = rig
            .engine
            .apply_config(&desired(&users, sni))
            .await
            .unwrap_err();
        assert!(matches!(error, EngineError::Config(_)), "{error}");
        assert!(!error.to_string().contains("not-a-secret"), "{error}");
    }
    let only_pool = CONFIG_TOML.replace(&format!("owner = \"{OWNER}\"\n"), "");
    let rig = rig_with(&only_pool, SETTINGS, Some(r#"["alice"]"#), |_| {
        ScriptedHttp::new()
    });
    let error = rig.engine.apply_config(&desired(&[], "example.com")).await;
    assert!(matches!(error, Err(EngineError::Config(_))), "{error:?}");
    assert!(rig.runner.calls().is_empty());
}

/// A real MTProxyL node under load, with the pool's user and the owner's one.
#[tokio::test]
async fn mtproxyl_counts_the_pools_user_of_the_recorded_answer() {
    let rig = rig_with(CONFIG_TOML, SETTINGS, Some(r#"["POOL"]"#), |_| {
        let http = ScriptedHttp::new();
        http.always(METRICS_URL, ok(METRICS_USERS));
        http
    });
    let report = rig.engine.metrics().await.unwrap();
    assert_eq!(report.from_client_total, 641_765_116);
    assert_eq!(report.to_client_total, 20_564_997_756);
    assert_eq!(report.clients, Some(11));
    // The owner's user has no octets series at all: that is zero, not a fault.
    assert_eq!(report.other_from_client_total, Some(0));
    assert_eq!(report.other_to_client_total, Some(0));
    assert_eq!(report.other_clients, Some(0));
}

/// The same recording before the pool's users are known.
#[tokio::test]
async fn mtproxyl_counts_nothing_of_the_recorded_answer_without_the_list() {
    let rig = rig_with(CONFIG_TOML, SETTINGS, None, |_| {
        let http = ScriptedHttp::new();
        http.always(METRICS_URL, ok(METRICS_USERS));
        http
    });
    let report = rig.engine.metrics().await.unwrap();
    assert_eq!(report.from_client_total, 0);
    assert_eq!(report.to_client_total, 0);
    assert_eq!(report.clients, None);
    assert_eq!(report.other_from_client_total, None);
}

/// MEKO runs telemt: the same recording, the same split.
#[tokio::test]
async fn meko_counts_by_the_same_list() {
    let fs = Arc::new(MemFs::new());
    put(&fs, MEKO_CONFIG, CONFIG_TOML);
    put(&fs, POOL_USERS, r#"["POOL"]"#);
    let http = ScriptedHttp::new();
    http.always(METRICS_URL, ok(METRICS_USERS));
    let engine = MekoEngine::new(
        MEKO_CONFIG.into(),
        "telemt.service",
        STATE_DIR.into(),
        Arc::new(ScriptedRunner::new()),
        fs,
        Arc::new(http),
    );
    let report = engine.metrics().await.unwrap();
    assert_eq!(report.from_client_total, 641_765_116);
    assert_eq!(report.clients, Some(11));
    assert_eq!(report.other_clients, Some(0));
}

#[tokio::test]
async fn mtproxyl_reads_its_metrics_port_and_dc2_like_telemt() {
    let rig = rig(CONFIG_TOML, Some(r#"["probe"]"#));
    assert_eq!(rig.engine.kind(), EngineKind::MtProxyL);
    assert_eq!(rig.engine.port(), Some(443));
    let report = rig.engine.metrics().await.unwrap();
    assert!(report.ok);
    assert_eq!(report.clients, Some(0));
    assert!(rig.engine.health().await.ok);
    assert!(rig.engine.dc2().await.is_some());
    assert_eq!(
        rig.engine.detect().await,
        Some(DetectedEngine {
            engine: EngineKind::MtProxyL,
            port: 443
        })
    );
}

#[tokio::test]
async fn mtproxyl_finds_the_config_in_the_order_of_the_candidates() {
    assert_eq!(
        mtproxyl::CONFIG_CANDIDATES,
        [
            "/opt/mtproxyl/mtproxy/config.toml",
            "/opt/mtproxyl/mtproxy/telemt.toml",
            "/opt/mtproxyl/superexpert.toml"
        ]
    );
    for (path, port) in [
        ("/opt/mtproxyl/mtproxy/telemt.toml", 8443),
        ("/opt/mtproxyl/superexpert.toml", 9443),
    ] {
        let fs = Arc::new(MemFs::new());
        put(
            &fs,
            path,
            &CONFIG_TOML.replace("port = 443", &format!("port = {port}")),
        );
        let engine = MtProxyLEngine::new(
            STATE_DIR.into(),
            Arc::new(ScriptedRunner::new()),
            fs.clone(),
            Arc::new(ScriptedHttp::new()),
        );
        assert_eq!(engine.port(), Some(port), "{path}");
        // The first one wins when more than one is there.
        put(&fs, "/opt/mtproxyl/mtproxy/config.toml", CONFIG_TOML);
        let engine = MtProxyLEngine::new(
            STATE_DIR.into(),
            Arc::new(ScriptedRunner::new()),
            fs,
            Arc::new(ScriptedHttp::new()),
        );
        assert_eq!(engine.port(), Some(443));
    }
    assert_eq!(mtproxyl::SETTINGS_PATH, SETTINGS_PATH);
}

#[tokio::test]
async fn mtproxyl_is_a_sink_for_the_registrars_config() {
    let rig = rig(CONFIG_TOML, None);
    push_ok(&rig.runner, 5);
    let config = AgentConfig {
        users: vec![AgentUser {
            name: "bob".into(),
            secret_hex: BOB.into(),
        }],
        sni: "cdn.example.org".into(),
        port: 443,
        intervals: Intervals {
            heartbeat_sec: 30,
            globalping_sec: 300,
            metrics_sec: 60,
            sync_sec: 60,
        },
        antiscan: true,
        report_other_users: false,
        measure_now: false,
    };
    rig.engine.apply(&config).await;
    assert_eq!(rig.cli().last().map(String::as_str), Some("restart"));
}

// ---------------------------------------------------------------------------------------------
// MEKO
// ---------------------------------------------------------------------------------------------

fn telemt_config() -> String {
    include_str!("fixtures/telemt/telemt.toml").to_owned()
}

#[tokio::test]
async fn meko_uses_telemt_service() {
    let fs = Arc::new(MemFs::new());
    put(&fs, MEKO_CONFIG, &telemt_config());
    let runner = Arc::new(ScriptedRunner::new());
    // `systemctl cat telemt.service` finds it.
    runner.push_output(0, b"[Unit]\n");
    let http = Arc::new(ScriptedHttp::new());
    http.always(METRICS_URL, ok(METRICS));
    let engine = MekoEngine::detect_unit(
        MEKO_CONFIG.into(),
        STATE_DIR.into(),
        runner.clone(),
        fs.clone(),
        http,
    )
    .await;

    assert_eq!(engine.kind(), EngineKind::Meko);
    // stop, start: the telemt adapter's transaction, on the unit that was found.
    push_ok(&runner, 2);
    let outcome = engine
        .apply_config(&desired(&[("bob", BOB)], "cdn.example.org"))
        .await
        .unwrap();
    assert_eq!(outcome, ApplyOutcome::Applied);
    let calls: Vec<_> = runner
        .calls()
        .iter()
        .map(|c| format!("{} {}", c.program, c.args.join(" ")))
        .collect();
    assert_eq!(
        calls,
        [
            "systemctl cat telemt.service",
            "systemctl stop telemt.service",
            "systemctl start telemt.service"
        ]
    );
    assert!(text_of(&fs, MEKO_CONFIG).unwrap().contains(BOB));
    assert_eq!(meko::DEFAULT_CONFIG_PATH, MEKO_CONFIG);
}

#[tokio::test]
async fn meko_takes_the_first_unit_systemd_knows_and_falls_back_to_telemt_service() {
    for (answers, expected) in [
        (vec![1, 0], "mtproxy.service"),
        (vec![1, 1, 0], "mtproxyl-telemt.service"),
        // None known: the unit MEKO installs is named telemt.service all the same.
        (vec![1, 1, 1], "telemt.service"),
    ] {
        let fs = Arc::new(MemFs::new());
        put(&fs, MEKO_CONFIG, &telemt_config());
        let runner = Arc::new(ScriptedRunner::new());
        for code in answers {
            runner.push_output(code, b"");
        }
        let http = Arc::new(ScriptedHttp::new());
        http.always(METRICS_URL, ok(METRICS));
        let engine = MekoEngine::detect_unit(
            MEKO_CONFIG.into(),
            STATE_DIR.into(),
            runner.clone(),
            fs,
            http,
        )
        .await;
        let before = runner.calls().len();
        push_ok(&runner, 2);
        engine
            .apply_config(&desired(&[("bob", BOB)], "cdn.example.org"))
            .await
            .unwrap();
        let stop = &runner.calls()[before];
        assert_eq!(stop.args, ["stop", expected]);
    }
}

#[tokio::test]
async fn meko_is_telemt_for_everything_but_its_name() {
    let fs = Arc::new(MemFs::new());
    put(&fs, MEKO_CONFIG, &telemt_config());
    let http = Arc::new(ScriptedHttp::new());
    http.always(METRICS_URL, ok(METRICS));
    http.always("http://127.0.0.1:9091/v1/stats/dcs", ok(DCS));
    let engine = MekoEngine::new(
        MEKO_CONFIG.into(),
        "telemt.service",
        STATE_DIR.into(),
        Arc::new(ScriptedRunner::new()),
        fs,
        http,
    )
    .with_clock(|| DCS_AT + 10);
    assert_eq!(engine.kind(), EngineKind::Meko);
    assert_eq!(engine.port(), Some(8443));
    assert!(engine.metrics().await.unwrap().ok);
    assert!(engine.health().await.ok);
    assert!(engine.dc2().await.is_some());
    assert_eq!(
        engine.detect().await,
        Some(DetectedEngine {
            engine: EngineKind::Meko,
            port: 8443
        })
    );
    // As a trait object, next to the others.
    let engines: Vec<Arc<dyn Engine>> = vec![Arc::new(engine)];
    assert_eq!(engines[0].kind(), EngineKind::Meko);
}
