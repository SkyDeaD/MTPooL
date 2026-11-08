//! Antiscan: the list, the ipset script, the iptables chain and the timing. Commands go to a
//! scripted runner and the list comes from a scripted source or a wiremock server; nothing runs
//! on the host.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use mtpool_agent::antiscan::testing::ScriptedList;
use mtpool_agent::antiscan::{
    Antiscan, AntiscanError, AntiscanSink, IpsumHttp, ListSource, MAX_DOWNLOAD_BYTES, MAX_ELEMENTS,
    REFRESH_EVERY, RETRY_AFTER, is_absent_error, parse_ipsum, render_ipset_restore,
};
use mtpool_agent::host::testing::{Call, ScriptedRunner};
use mtpool_agent::lifecycle::ConfigSink;
use mtpool_protocol::agent::{AgentConfig, AgentUser, Intervals};
use tokio::time::Instant;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LEVEL1: &str = include_str!("fixtures/antiscan/ipsum_level1.txt");
const SCORES: &str = include_str!("fixtures/antiscan/ipsum_scores.txt");
const ABSENT: &str = include_str!("fixtures/antiscan/host_errors.txt");
const REAL_ERRORS: &str = include_str!("fixtures/antiscan/host_errors_real.txt");

fn ip(text: &str) -> Ipv4Addr {
    text.parse().unwrap()
}

fn call(program: &str, args: &[&str]) -> Call {
    Call {
        program: program.into(),
        args: args.iter().map(|a| (*a).to_owned()).collect(),
        stdin: None,
    }
}

fn iptables(args: &[&str]) -> Call {
    let mut all = vec!["-w", "5"];
    all.extend_from_slice(args);
    call("iptables", &all)
}

fn rule_call(op: &str) -> Call {
    iptables(&[
        op,
        "ANTISCAN_MTPOOL",
        "-m",
        "set",
        "--match-set",
        "ANTISCAN_MTPOOL",
        "src",
        "-j",
        "DROP",
    ])
}

/// What a refresh runs when nothing is in place yet.
fn first_install(script: &ScriptedRunner) {
    script.push_output(0, b""); // ipset restore
    script.push_output(0, b""); // -N
    script.push_failure(
        1,
        "iptables: Bad rule (does a matching rule exist in that chain?).\n",
    );
    script.push_output(0, b""); // -A
    script.push_failure(
        1,
        "iptables: Bad rule (does a matching rule exist in that chain?).\n",
    );
    script.push_output(0, b""); // -I
}

fn list(addresses: &[&str]) -> String {
    addresses.iter().map(|a| format!("{a}\n")).collect()
}

fn antiscan_with(
    runner: &Arc<ScriptedRunner>,
    source: &Arc<ScriptedList>,
    enabled: bool,
) -> Antiscan<ScriptedRunner, Arc<ScriptedList>> {
    Antiscan::new(runner.clone(), source.clone(), enabled)
}

fn programs(runner: &ScriptedRunner) -> Vec<String> {
    runner
        .calls()
        .iter()
        .map(|c| format!("{} {}", c.program, c.args.join(" ")))
        .collect()
}

// ---- the list ----

#[test]
fn parses_ipsum_level1_ignores_comments() {
    // The recorded shape: one address per line.
    let ips = parse_ipsum(LEVEL1);
    assert_eq!(ips.len(), 14);
    assert_eq!(ips[0], ip("192.0.2.10"));
    assert_eq!(ips[13], ip("192.0.2.201"));

    // The shape of the full list: comments, blank lines, a tab and a score, noise.
    let ips = parse_ipsum(SCORES);
    assert_eq!(
        ips,
        [
            ip("192.0.2.10"),
            ip("198.51.100.20"),
            ip("203.0.113.5"),
            ip("198.51.100.21"),
            ip("203.0.113.6"),
        ],
        "comments, scores, duplicates, IPv6, junk and non-public addresses are left out"
    );
    assert!(parse_ipsum("").is_empty());
    assert!(parse_ipsum("# only a comment\n\n").is_empty());
    // Windows line ends do not break a line.
    assert_eq!(parse_ipsum("192.0.2.1\r\n192.0.2.2\r\n").len(), 2);
}

#[test]
fn restore_script_swaps_atomically() {
    let script = render_ipset_restore("ANTISCAN_MTPOOL", &[ip("192.0.2.1"), ip("198.51.100.2")]);
    assert_eq!(
        script,
        "create ANTISCAN_MTPOOL hash:ip family inet maxelem 262144 -exist\n\
         create ANTISCAN_MTPOOL_tmp hash:ip family inet maxelem 262144 -exist\n\
         flush ANTISCAN_MTPOOL_tmp\n\
         add ANTISCAN_MTPOOL_tmp 192.0.2.1\n\
         add ANTISCAN_MTPOOL_tmp 198.51.100.2\n\
         swap ANTISCAN_MTPOOL_tmp ANTISCAN_MTPOOL\n\
         destroy ANTISCAN_MTPOOL_tmp\n"
    );
    assert_eq!(MAX_ELEMENTS, 262_144);
}

#[tokio::test]
async fn the_script_goes_to_one_ipset_restore_through_stdin() {
    let runner = Arc::new(ScriptedRunner::new());
    first_install(&runner);
    let source = Arc::new(ScriptedList::new());
    source.push(Ok(list(&["192.0.2.1", "198.51.100.2"])));
    let antiscan = antiscan_with(&runner, &source, true);

    assert_eq!(antiscan.refresh().await, Ok(2));

    let calls = runner.calls();
    let restores: Vec<&Call> = calls.iter().filter(|c| c.program == "ipset").collect();
    assert_eq!(restores.len(), 1, "one command for the whole list");
    assert_eq!(restores[0].args, ["restore"]);
    let expected = render_ipset_restore("ANTISCAN_MTPOOL", &[ip("192.0.2.1"), ip("198.51.100.2")]);
    assert_eq!(restores[0].stdin.as_deref(), Some(expected.as_bytes()));
    // The set exists before the chain refers to it.
    assert_eq!(calls[0].program, "ipset");
}

#[tokio::test]
async fn a_list_longer_than_a_set_is_cut() {
    let runner = Arc::new(ScriptedRunner::new());
    first_install(&runner);
    let source = Arc::new(ScriptedList::new());
    let many: String = (0..MAX_ELEMENTS + 10)
        .map(|n| format!("198.{}.{}.{}\n", 18 + (n >> 16), (n >> 8) % 256, n % 256))
        .collect();
    source.push(Ok(many));
    let antiscan = antiscan_with(&runner, &source, true);
    assert_eq!(antiscan.refresh().await, Ok(MAX_ELEMENTS));
    let stdin = runner.calls()[0].stdin.clone().unwrap();
    let adds = String::from_utf8(stdin)
        .unwrap()
        .lines()
        .filter(|l| l.starts_with("add "))
        .count();
    assert_eq!(adds, MAX_ELEMENTS);
}

// ---- iptables ----

#[tokio::test]
async fn iptables_chain_idempotent() {
    let runner = Arc::new(ScriptedRunner::new());
    let source = Arc::new(ScriptedList::new());
    source.push(Ok(list(&["192.0.2.1"])));
    source.push(Ok(list(&["192.0.2.1", "192.0.2.2"])));
    let antiscan = antiscan_with(&runner, &source, true);

    first_install(&runner);
    antiscan.refresh().await.unwrap();
    let first: Vec<Call> = runner.calls().into_iter().skip(1).collect();
    assert_eq!(
        first,
        [
            iptables(&["-N", "ANTISCAN_MTPOOL"]),
            rule_call("-C"),
            rule_call("-A"),
            iptables(&["-C", "INPUT", "-j", "ANTISCAN_MTPOOL"]),
            iptables(&["-I", "INPUT", "-j", "ANTISCAN_MTPOOL"]),
        ]
    );

    // Second round: the chain exists, both checks pass, nothing is added.
    runner.push_output(0, b""); // ipset restore
    runner.push_failure(1, "iptables: Chain already exists.\n"); // -N
    runner.push_output(0, b""); // -C rule
    runner.push_output(0, b""); // -C jump
    antiscan.refresh().await.unwrap();
    let second: Vec<String> = programs(&runner).into_iter().skip(6).collect();
    assert_eq!(
        second,
        [
            "ipset restore",
            "iptables -w 5 -N ANTISCAN_MTPOOL",
            "iptables -w 5 -C ANTISCAN_MTPOOL -m set --match-set ANTISCAN_MTPOOL src -j DROP",
            "iptables -w 5 -C INPUT -j ANTISCAN_MTPOOL",
        ]
    );
}

#[tokio::test]
async fn a_failed_restore_leaves_iptables_alone() {
    let runner = Arc::new(ScriptedRunner::new());
    runner.push_failure(1, "ipset v7.19: Error in line 4: Syntax error\n");
    let source = Arc::new(ScriptedList::new());
    source.push(Ok(list(&["192.0.2.1"])));
    let antiscan = antiscan_with(&runner, &source, true);
    let error = antiscan.refresh().await.unwrap_err();
    assert!(matches!(error, AntiscanError::Host(_)), "{error:?}");
    assert_eq!(runner.calls().len(), 1);
}

#[tokio::test]
async fn a_rule_that_cannot_be_added_is_an_error_and_is_tried_again() {
    let runner = Arc::new(ScriptedRunner::new());
    runner.push_output(0, b""); // restore
    runner.push_output(0, b""); // -N
    runner.push_failure(1, "no rule\n"); // -C rule
    runner.push_failure(4, "Another app is currently holding the xtables lock\n"); // -A
    let source = Arc::new(ScriptedList::new());
    source.push(Ok(list(&["192.0.2.1"])));
    let antiscan = antiscan_with(&runner, &source, true);
    let start = Instant::now();
    antiscan.tick(start).await;
    assert_eq!(
        runner.calls().len(),
        4,
        "the jump is not added without the rule"
    );

    // Not installed: it is retried after the short delay, not after 30 minutes.
    let before = runner.calls().len();
    antiscan
        .tick(start + RETRY_AFTER - Duration::from_secs(1))
        .await;
    assert_eq!(runner.calls().len(), before);
    source.push(Ok(list(&["192.0.2.1"])));
    first_install(&runner);
    antiscan.tick(start + RETRY_AFTER).await;
    assert_eq!(source.fetched(), 2);
    assert!(runner.calls().len() > before);
}

#[tokio::test]
async fn a_host_without_ipset_is_an_error_not_a_panic() {
    let runner = Arc::new(ScriptedRunner::new());
    runner.push_error(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "No such file or directory",
    ));
    let source = Arc::new(ScriptedList::new());
    source.push(Ok(list(&["192.0.2.1"])));
    let antiscan = antiscan_with(&runner, &source, true);
    assert!(matches!(
        antiscan.refresh().await,
        Err(AntiscanError::Host(_))
    ));
}

// ---- timing ----

#[tokio::test]
async fn refresh_every_30min() {
    assert_eq!(REFRESH_EVERY, Duration::from_secs(1800));
    let runner = Arc::new(ScriptedRunner::new());
    let source = Arc::new(ScriptedList::new());
    let antiscan = antiscan_with(&runner, &source, true);
    let t0 = Instant::now();

    source.push(Ok(list(&["192.0.2.1"])));
    first_install(&runner);
    antiscan.tick(t0).await;
    assert_eq!(source.fetched(), 1, "the first tick downloads at once");

    antiscan.tick(t0 + Duration::from_secs(60)).await;
    antiscan
        .tick(t0 + REFRESH_EVERY - Duration::from_secs(1))
        .await;
    assert_eq!(source.fetched(), 1, "nothing before 30 minutes");

    source.push(Ok(list(&["192.0.2.1", "192.0.2.2"])));
    runner.push_output(0, b"");
    runner.push_failure(1, "iptables: Chain already exists.\n");
    runner.push_output(0, b"");
    runner.push_output(0, b"");
    antiscan.tick(t0 + REFRESH_EVERY).await;
    assert_eq!(source.fetched(), 2, "again after 30 minutes");

    antiscan
        .tick(t0 + REFRESH_EVERY + Duration::from_secs(60))
        .await;
    assert_eq!(source.fetched(), 2);
}

// ---- failures keep the old list ----

#[tokio::test]
async fn download_failure_keeps_old_list() {
    let runner = Arc::new(ScriptedRunner::new());
    let source = Arc::new(ScriptedList::new());
    let antiscan = antiscan_with(&runner, &source, true);
    let t0 = Instant::now();

    source.push(Ok(list(&["192.0.2.1"])));
    first_install(&runner);
    antiscan.tick(t0).await;
    let installed = runner.calls().len();

    // A failed download, an empty body, a page that is not a list: no command, the set stays.
    for bad in [
        Err("connection refused".to_owned()),
        Ok(String::new()),
        Ok("<html>Service Unavailable</html>\n".to_owned()),
    ] {
        source.push(bad);
    }
    let mut at = t0 + REFRESH_EVERY;
    for _ in 0..3 {
        antiscan.tick(at).await;
        assert_eq!(runner.calls().len(), installed, "the old list is untouched");
        // A failure is retried soon, not in 30 minutes.
        at += RETRY_AFTER;
    }
    assert_eq!(source.fetched(), 4);

    assert_eq!(
        antiscan.refresh().await,
        Err(AntiscanError::Download("no scripted list left".into()))
    );
    assert_eq!(runner.calls().len(), installed);
}

#[tokio::test]
async fn an_empty_list_is_an_error() {
    let runner = Arc::new(ScriptedRunner::new());
    let source = Arc::new(ScriptedList::new());
    source.push(Ok("# nothing here\n".into()));
    let antiscan = antiscan_with(&runner, &source, true);
    assert_eq!(antiscan.refresh().await, Err(AntiscanError::Empty));
    assert!(runner.calls().is_empty());
}

// ---- switching off ----

fn removal(script: &ScriptedRunner) {
    script.push_output(0, b""); // -D jump
    script.push_failure(
        1,
        "iptables: Bad rule (does a matching rule exist in that chain?).\n",
    );
    script.push_output(0, b""); // -F
    script.push_output(0, b""); // -X
    script.push_output(0, b""); // destroy set
    script.push_failure(
        1,
        "ipset v7.19: The set with the given name does not exist\n",
    );
}

#[tokio::test]
async fn disabled_removes_rules() {
    let runner = Arc::new(ScriptedRunner::new());
    let source = Arc::new(ScriptedList::new());
    let antiscan = antiscan_with(&runner, &source, false);
    removal(&runner);
    let t0 = Instant::now();
    antiscan.tick(t0).await;

    assert_eq!(
        programs(&runner),
        [
            "iptables -w 5 -D INPUT -j ANTISCAN_MTPOOL",
            "iptables -w 5 -D INPUT -j ANTISCAN_MTPOOL",
            "iptables -w 5 -F ANTISCAN_MTPOOL",
            "iptables -w 5 -X ANTISCAN_MTPOOL",
            "ipset destroy ANTISCAN_MTPOOL",
            "ipset destroy ANTISCAN_MTPOOL_tmp",
        ],
        "jump first, then the chain, then the sets, which are in use until the chain is gone"
    );
    assert_eq!(source.fetched(), 0, "nothing is downloaded while disabled");

    // Once is enough while it stays off.
    let done = runner.calls().len();
    antiscan.tick(t0 + REFRESH_EVERY * 3).await;
    assert_eq!(runner.calls().len(), done);
}

#[tokio::test]
async fn switching_off_removes_what_was_installed_and_switching_on_installs_again() {
    let runner = Arc::new(ScriptedRunner::new());
    let source = Arc::new(ScriptedList::new());
    let antiscan = antiscan_with(&runner, &source, true);
    let t0 = Instant::now();

    source.push(Ok(list(&["192.0.2.1"])));
    first_install(&runner);
    antiscan.tick(t0).await;
    let installed = runner.calls().len();

    antiscan.set_enabled(false);
    removal(&runner);
    antiscan.tick(t0 + Duration::from_secs(1)).await;
    assert_eq!(runner.calls().len(), installed + 6);

    // On again: the list is downloaded at once, not when the old timer says.
    antiscan.set_enabled(true);
    source.push(Ok(list(&["192.0.2.1"])));
    first_install(&runner);
    antiscan.tick(t0 + Duration::from_secs(2)).await;
    assert_eq!(source.fetched(), 2);
    assert_eq!(runner.calls().len(), installed + 12);
}

#[tokio::test]
async fn refresh_does_nothing_when_disabled() {
    let runner = Arc::new(ScriptedRunner::new());
    let source = Arc::new(ScriptedList::new());
    let antiscan = antiscan_with(&runner, &source, false);
    assert_eq!(antiscan.refresh().await, Ok(0));
    assert!(runner.calls().is_empty());
    assert_eq!(source.fetched(), 0);
}

#[tokio::test]
async fn a_removal_that_fails_is_tried_again_soon() {
    // A host where the commands do not run: nothing panics, the agent goes on, and the removal
    // is repeated after the short delay.
    let runner = Arc::new(ScriptedRunner::new());
    let source = Arc::new(ScriptedList::new());
    let antiscan = antiscan_with(&runner, &source, false);
    let t0 = Instant::now();
    antiscan.tick(t0).await;
    let first = runner.calls().len();
    assert!(first > 0);
    antiscan
        .tick(t0 + RETRY_AFTER - Duration::from_secs(1))
        .await;
    assert_eq!(runner.calls().len(), first);
    antiscan.tick(t0 + RETRY_AFTER).await;
    assert!(runner.calls().len() > first);
}

// ---- the sink ----

fn config(antiscan: bool) -> AgentConfig {
    AgentConfig {
        users: vec![AgentUser {
            name: "u1".into(),
            secret_hex: "00112233445566778899aabbccddeeff".into(),
        }],
        sni: "tls.example.test".into(),
        port: 443,
        intervals: Intervals {
            heartbeat_sec: 60,
            globalping_sec: 300,
            metrics_sec: 60,
            sync_sec: 60,
        },
        antiscan,
        report_other_users: false,
        measure_now: false,
        report_check_host: false,
        check_host_now: false,
    }
}

#[derive(Clone, Default)]
struct Seen(Arc<std::sync::Mutex<u32>>);

impl ConfigSink for Seen {
    async fn apply(&self, _config: &AgentConfig) {
        *self.0.lock().unwrap() += 1;
    }
}

#[tokio::test]
async fn the_sink_follows_the_flag_of_the_config_and_of_the_host() {
    let runner = Arc::new(ScriptedRunner::new());
    let source = Arc::new(ScriptedList::new());
    let antiscan = Arc::new(antiscan_with(&runner, &source, true));
    let seen = Seen::default();
    let sink = AntiscanSink {
        antiscan: antiscan.clone(),
        local_enabled: true,
        inner: seen.clone(),
    };

    // The registrar switches it off: the next tick removes the rules.
    sink.apply(&config(false)).await;
    removal(&runner);
    antiscan.tick(Instant::now()).await;
    assert_eq!(runner.calls().len(), 6);
    assert_eq!(
        *seen.0.lock().unwrap(),
        1,
        "the inner sink gets the config too"
    );

    // The registrar switches it on, but the local config says no.
    let local_off = AntiscanSink {
        antiscan: antiscan.clone(),
        local_enabled: false,
        inner: seen.clone(),
    };
    local_off.apply(&config(true)).await;
    antiscan.tick(Instant::now()).await;
    assert_eq!(runner.calls().len(), 6);
    assert_eq!(source.fetched(), 0);

    sink.apply(&config(true)).await;
    source.push(Ok(list(&["192.0.2.1"])));
    first_install(&runner);
    antiscan.tick(Instant::now()).await;
    assert_eq!(source.fetched(), 1);
}

// ---- errors of host commands ----

#[test]
fn recorded_errors_of_absent_things_are_not_problems() {
    for line in ABSENT.lines() {
        assert!(is_absent_error(line), "{line}");
    }
    for line in REAL_ERRORS.lines() {
        assert!(!is_absent_error(line), "{line}");
    }
    // Made up: a run without the rights for it.
    assert!(!is_absent_error(
        "iptables v1.8.10 (nf_tables): Could not fetch rule set generation id: Permission denied (you must be root)"
    ));
}

// ---- the real source over HTTP ----

async fn serve(response: ResponseTemplate) -> (MockServer, IpsumHttp) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/levels/1.txt"))
        .respond_with(response)
        .mount(&server)
        .await;
    let http = IpsumHttp::with_url(&format!("{}/levels/1.txt", server.uri())).unwrap();
    (server, http)
}

#[tokio::test]
async fn the_list_is_downloaded_over_http() {
    let (_server, http) = serve(ResponseTemplate::new(200).set_body_string(LEVEL1)).await;
    assert_eq!(http.fetch().await.unwrap(), LEVEL1);
}

#[tokio::test]
async fn an_error_status_is_a_failed_download() {
    let (_server, http) = serve(ResponseTemplate::new(404).set_body_string("404: Not Found")).await;
    let error = http.fetch().await.unwrap_err();
    assert!(error.contains("404"), "{error}");
    let (_server, http) = serve(ResponseTemplate::new(503)).await;
    assert!(http.fetch().await.is_err());
}

#[tokio::test]
async fn a_redirect_is_not_followed() {
    let (_server, http) =
        serve(ResponseTemplate::new(302).insert_header("location", "http://127.0.0.1:9/x")).await;
    assert!(http.fetch().await.is_err());
}

#[tokio::test]
async fn a_list_over_the_cap_is_refused() {
    let body = "192.0.2.1\n".repeat(MAX_DOWNLOAD_BYTES / 10 + 10);
    assert!(body.len() > MAX_DOWNLOAD_BYTES);
    let (_server, http) = serve(ResponseTemplate::new(200).set_body_string(body)).await;
    let error = http.fetch().await.unwrap_err();
    assert!(
        error.contains("too long") || error.contains("exceeds"),
        "{error}"
    );
}

#[tokio::test]
async fn bytes_that_are_not_text_are_a_failed_download() {
    let (_server, http) =
        serve(ResponseTemplate::new(200).set_body_bytes(vec![0xff, 0xfe, 0x00, 0x41])).await;
    assert!(http.fetch().await.is_err());
}

#[tokio::test]
async fn an_unreachable_server_is_a_failed_download() {
    let http = IpsumHttp::with_url("http://127.0.0.1:1/levels/1.txt").unwrap();
    assert!(http.fetch().await.is_err());
}
