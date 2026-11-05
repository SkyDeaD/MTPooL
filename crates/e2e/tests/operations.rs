//! The operator's scenarios: installing a node, backing the registrar up and restoring it, and
//! the traffic counters. Needs Docker (and, for the installers, the internet: they install
//! `minisign` and the engine from the distribution and from GitHub), so all are ignored by
//! default: `cargo test -p mtpool-e2e --test operations -- --ignored`.

mod common;

use std::time::Instant;

use common::{enroll_healthy, run_registrar_build, secs, settings};
use mtpool_e2e::{
    InstallOptions, PROXY_USER_SECRET, ReleaseSpec, Stand, TLS_DOMAIN, TestKey, Token,
    ensure_test_key, eventually, registrar_release,
};
use serde_json::Value;

/// Offers the agent of node 1's image as a release signed by the stand's key, under the version
/// that build reports: not newer than the stand's registrar, so the installer gets it.
async fn offer_release(stand: &Stand, key: &TestKey) {
    let node = stand.node(1);
    let binary = node.fetch_agent_binary().await.expect("the agent build");
    let target = node.target().await.expect("the target");
    stand
        .sign_release(&ReleaseSpec {
            version: "0.0.0",
            agent_binary: &binary,
            target: &target,
            key,
        })
        .await
        .expect("the release is put on the registrar");
}

#[tokio::test]
#[ignore = "needs Docker and the internet; run with --ignored"]
async fn install_rejects_bad_signature() {
    let stand = Stand::up().await.expect("the stand comes up");
    let foreign = TestKey::generate().expect("a key the installer does not know");
    offer_release(&stand, &foreign).await;
    let node = stand.node(1);
    let run = node
        .install(&InstallOptions {
            name: "node-one",
            token: Token::Env(stand.enroll_token()),
            preset: None,
            port: None,
        })
        .await
        .expect("the installer runs");
    assert!(!run.success, "the installer must refuse:\n{}", run.output);
    assert!(
        run.output.to_lowercase().contains("подпись"),
        "it says why:\n{}",
        run.output
    );
    let left = node
        .exec(
            "ls /var/lib/mtpool/enroll_token /etc/mtpool/agent.toml /usr/local/bin/mtpool-agent.new \
             2>&1 | grep -v 'No such file' || true; \
             systemctl is-enabled mtpool-agent 2>&1 || true",
        )
        .await
        .unwrap();
    assert!(
        !left.contains("enroll_token")
            && !left.contains("agent.toml")
            && !left.contains("enabled\n"),
        "nothing of the node is set up: {left}"
    );
    assert!(
        node.node_id().await.unwrap().is_none(),
        "the node did not enroll"
    );
}

/// The stand's registrar is 0.9.0 (`e2e/images/registrar/Dockerfile`), and 0.9.1 is its next build.
#[tokio::test]
#[ignore = "needs Docker and the internet; run with --ignored"]
async fn install_waits_for_a_registrar_that_was_rolled_back() {
    let stand = Stand::up().await.expect("the stand comes up");
    let key = ensure_test_key().expect("the stand's key");
    let node = stand.node(1);
    let kept = stand
        .registrar_exec("cp /usr/local/bin/mtpool-registrar /opt/e2e/registrar-0.9.0")
        .await
        .expect("the old build is kept");
    assert!(kept.success, "{}", kept.output);
    let build = node
        .fetch_file("/opt/e2e/agent-0.9.1")
        .await
        .expect("the 0.9.1 build");
    let target = node.target().await.expect("the target");
    stand
        .sign_release(&ReleaseSpec {
            version: "0.9.1",
            agent_binary: &build,
            target: &target,
            key: &key,
        })
        .await
        .expect("the release is put on the registrar");

    // The update and the rollback: the manifest of 0.9.1 stays, the only agent build beside it.
    run_registrar_build(&stand, "0.9.1").await;
    run_registrar_build(&stand, "0.9.0").await;
    let options = InstallOptions {
        name: "node-one",
        token: Token::Env(stand.enroll_token()),
        preset: None,
        port: None,
    };
    let run = node.install(&options).await.expect("the installer runs");
    assert!(!run.success, "the installer must refuse:\n{}", run.output);
    assert!(
        run.output.contains("релиз на пуле новее его регистратора"),
        "it says why:\n{}",
        run.output
    );
    assert!(
        run.output.contains("когда регистратор обновится"),
        "it says what to do:\n{}",
        run.output
    );
    let left = node
        .exec(
            "ls /var/lib/mtpool/enroll_token /etc/mtpool/agent.toml /usr/local/bin/mtpool-agent.new \
             2>&1 | grep -v 'No such file' || true; \
             systemctl is-enabled mtpool-agent 2>&1 || true; mtpool-agent version",
        )
        .await
        .unwrap();
    assert!(
        !left.contains("enroll_token")
            && !left.contains("agent.toml")
            && !left.contains("enabled\n")
            && !left.contains("0.9.1"),
        "nothing of the node is set up: {left}"
    );
    assert!(
        node.node_id().await.unwrap().is_none(),
        "the node did not enroll"
    );

    // The registrar reaches that version: the same command works, with the new agent.
    run_registrar_build(&stand, "0.9.1").await;
    let run = node.install(&options).await.expect("the installer runs");
    assert!(run.success, "the installer works:\n{}", run.output);
    let id = node.wait_node_id(secs(60)).await.expect("the node enrolls");
    stand
        .wait_state(&id, "healthy", secs(120))
        .await
        .expect("the node is healthy");
    let version = node
        .exec("/usr/local/bin/mtpool-agent version")
        .await
        .expect("the agent answers");
    assert!(version.starts_with("0.9.1"), "{version}");
}

#[tokio::test]
#[ignore = "needs Docker and the internet; run with --ignored"]
async fn install_rejects_port_mismatch() {
    let stand = Stand::up().await.expect("the stand comes up");
    offer_release(&stand, &ensure_test_key().expect("the test key")).await;
    let node = stand.node(1);
    let run = node
        .install(&InstallOptions {
            name: "node-one",
            token: Token::Env(stand.enroll_token()),
            preset: None,
            port: Some(8443),
        })
        .await
        .expect("the installer runs");
    assert!(!run.success, "the installer must refuse:\n{}", run.output);
    assert!(
        run.output.contains("слушает порт 8443, а пул ждёт 443"),
        "it names both ports:\n{}",
        run.output
    );
    assert!(
        node.node_id().await.unwrap().is_none(),
        "the node did not enroll"
    );
}

#[tokio::test]
#[ignore = "needs Docker and the internet; run with --ignored"]
async fn token_not_visible_in_ps() {
    let stand = Stand::up().await.expect("the stand comes up");
    offer_release(&stand, &ensure_test_key().expect("the test key")).await;
    let node = stand.node(1);
    let token = stand.enroll_token().to_owned();
    // There is no `ps` on the node: the command lines are read from /proc, as `ps` would.
    node.exec("rm -f /tmp/install.done").await.unwrap();
    let sampler = node.try_exec(
        "while [ ! -e /tmp/install.done ]; do cat /proc/[0-9]*/cmdline 2>/dev/null | tr '\\0' '\\n'; \
         sleep 0.02; done | sort -u",
        &[],
    );
    let install = async {
        let run = node
            .install(&InstallOptions {
                name: "node-one",
                token: Token::Env(&token),
                preset: None,
                port: None,
            })
            .await;
        node.exec("touch /tmp/install.done").await.unwrap();
        run
    };
    let (sampled, installed) = tokio::join!(sampler, install);
    let installed = installed.expect("the installer runs");
    assert!(
        installed.success,
        "the installer works:\n{}",
        installed.output
    );
    let sampled = sampled.expect("the sampler runs").output;
    assert!(
        sampled.contains("node-one"),
        "the sampler saw the installer's command line (it works):\n{sampled}"
    );
    assert!(
        !sampled.contains(&token),
        "the enroll token was in a command line on the node"
    );
    node.wait_node_id(secs(60)).await.expect("the node enrolls");
}

#[tokio::test]
#[ignore = "needs Docker and the internet; run with --ignored"]
async fn token_file_option_works() {
    let stand = Stand::up().await.expect("the stand comes up");
    offer_release(&stand, &ensure_test_key().expect("the test key")).await;
    let node = stand.node(1);
    node.try_exec(
        "umask 077 && printf %s \"$T\" > /root/enroll.tok",
        &[("T", stand.enroll_token())],
    )
    .await
    .unwrap();
    let run = node
        .install(&InstallOptions {
            name: "node-one",
            token: Token::File("/root/enroll.tok"),
            preset: None,
            port: None,
        })
        .await
        .expect("the installer runs");
    assert!(run.success, "the installer works:\n{}", run.output);
    let id = node.wait_node_id(secs(60)).await.expect("the node enrolls");
    stand
        .wait_state(&id, "healthy", secs(120))
        .await
        .expect("the node is healthy");
    // It waited for the pool and says where to look next.
    assert!(
        run.output.contains(&format!("Готово: {id}")),
        "it reports a healthy node:\n{}",
        run.output
    );
    assert!(
        run.output.contains("journalctl -u mtpool-agent -f"),
        "it says where the logs are:\n{}",
        run.output
    );
    assert!(
        !run.output.contains('\u{1b}'),
        "no colors without a terminal:\n{}",
        run.output
    );
    let log = node
        .exec("stat -c %a /var/log/mtpool/install.log; cat /var/log/mtpool/install.log")
        .await
        .unwrap();
    assert!(log.starts_with("600\n"), "the log is for root alone: {log}");
    assert!(
        !log.contains(stand.enroll_token()),
        "the enroll token is not in the log"
    );
}

#[tokio::test]
#[ignore = "needs Docker and the internet; run with --ignored"]
async fn install_says_when_the_pool_refuses_the_node() {
    let stand = Stand::up().await.expect("the stand comes up");
    offer_release(&stand, &ensure_test_key().expect("the test key")).await;
    let node = stand.node(1);
    let run = node
        .install(&InstallOptions {
            name: "node-one",
            token: Token::Env("mtp_enroll_not_a_real_token"),
            preset: None,
            port: None,
        })
        .await
        .expect("the installer runs");
    assert!(
        !run.success,
        "a node outside the pool is no success:\n{}",
        run.output
    );
    assert!(
        run.output.contains("пул не принял ноду"),
        "it says what happened:\n{}",
        run.output
    );
    assert!(
        run.output.contains("/var/log/mtpool/install.log"),
        "it says where the log is:\n{}",
        run.output
    );
}

#[tokio::test]
#[ignore = "needs Docker and the internet; run with --ignored"]
async fn preset_telemt_installs_engine() {
    let stand = Stand::up().await.expect("the stand comes up");
    offer_release(&stand, &ensure_test_key().expect("the test key")).await;
    let node = stand.node(1);
    node.exec(
        "systemctl disable --now telemt; rm -f /usr/local/bin/telemt /etc/systemd/system/telemt.service \
         /etc/systemd/system/multi-user.target.wants/telemt.service; rm -rf /etc/telemt; \
         systemctl daemon-reload",
    )
    .await
    .unwrap();
    let without = node
        .try_exec("test -e /usr/local/bin/telemt", &[])
        .await
        .unwrap();
    assert!(!without.success, "the engine is gone before the install");
    let run = node
        .install(&InstallOptions {
            name: "node-one",
            token: Token::Env(stand.enroll_token()),
            preset: Some("telemt"),
            port: None,
        })
        .await
        .expect("the installer runs");
    assert!(run.success, "the installer works:\n{}", run.output);
    let active = node.exec("systemctl is-active telemt").await.unwrap();
    assert_eq!(active.trim(), "active", "the engine runs");
    let id = node.wait_node_id(secs(60)).await.expect("the node enrolls");
    stand
        .wait_state(&id, "healthy", secs(180))
        .await
        .expect("the node is healthy");
}

#[tokio::test]
#[ignore = "needs Docker and the internet; run with --ignored"]
async fn preset_mtproxyl_installs_engine() {
    let stand = Stand::up().await.expect("the stand comes up");
    offer_release(&stand, &ensure_test_key().expect("the test key")).await;
    // MTProxyL's firewall lets one source address open about one connection a second (54 a minute)
    // and rejects the rest; the stand probes every second, the pool's default is every 5.
    settings(&stand, serde_json::json!({"tcp_probe_interval_sec": 5})).await;
    let node = stand.node(1);
    node.exec(
        "systemctl disable --now telemt; rm -f /usr/local/bin/telemt /etc/systemd/system/telemt.service \
         /etc/systemd/system/multi-user.target.wants/telemt.service; rm -rf /etc/telemt; \
         systemctl daemon-reload",
    )
    .await
    .unwrap();
    let before = node.try_exec("test -e /opt/mtproxyl", &[]).await.unwrap();
    assert!(!before.success, "MTProxyL is not there before the install");
    let run = node
        .install(&InstallOptions {
            name: "node-one",
            token: Token::Env(stand.enroll_token()),
            preset: Some("mtproxyl"),
            port: None,
        })
        .await
        .expect("the installer runs");
    assert!(run.success, "the installer works:\n{}", run.output);
    let found = node
        .exec("test -x /opt/mtproxyl/mtproxyl.sh && command -v mtproxyl")
        .await
        .unwrap();
    assert!(
        found.contains("mtproxyl"),
        "the manager is installed: {found}"
    );
    let id = node.wait_node_id(secs(60)).await.expect("the node enrolls");
    stand
        .wait_state(&id, "healthy", secs(180))
        .await
        .expect("the node is healthy");
}

async fn node_ids(stand: &Stand) -> Vec<String> {
    let mut ids: Vec<String> = stand
        .nodes()
        .await
        .unwrap()
        .iter()
        .filter_map(|n| n["id"].as_str().map(str::to_owned))
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
#[ignore = "needs Docker; run with --ignored"]
async fn backup_then_restore_equal_state() {
    let stand = Stand::up().await.expect("the stand comes up");
    let (_node, id) = enroll_healthy(&stand, 1, "node-one").await;
    let settings_before: Value = stand.admin.get("/api/v1/admin/settings").await.unwrap();
    let nodes_before = node_ids(&stand).await;
    let bans_before = stand.bans().await.unwrap();

    let backup = stand
        .registrar_exec("mtpool-registrar backup --to /var/lib/mtpool/b.db && ls /var/lib/mtpool")
        .await
        .unwrap();
    assert!(backup.success, "the backup is made:\n{}", backup.output);
    assert!(
        backup.output.contains("b.db.sha256"),
        "the backup has its checksum:\n{}",
        backup.output
    );

    // The state moves on: a setting is changed and another node joins.
    settings(&stand, serde_json::json!({"reverify_pause_sec": 7})).await;
    let moved: Value = stand.admin.get("/api/v1/admin/settings").await.unwrap();
    assert_ne!(moved, settings_before, "the settings did change");
    let (_node2, _id2) = enroll_healthy(&stand, 2, "node-two").await;
    assert_ne!(node_ids(&stand).await, nodes_before);

    // A running registrar is never restored over.
    let refused = stand
        .registrar_exec("mtpool-registrar restore --from /var/lib/mtpool/b.db")
        .await
        .unwrap();
    assert!(!refused.success, "restore refuses: {}", refused.output);
    assert!(
        refused.output.contains("running"),
        "it says the registrar is running:\n{}",
        refused.output
    );

    let restored = stand
        .restore_registrar("/var/lib/mtpool/b.db")
        .await
        .unwrap();
    assert!(restored.success, "the restore works:\n{}", restored.output);

    let settings_after: Value = stand.admin.get("/api/v1/admin/settings").await.unwrap();
    assert_eq!(settings_after, settings_before, "the settings are back");
    assert_eq!(node_ids(&stand).await, nodes_before, "the nodes are back");
    assert_eq!(
        stand.bans().await.unwrap(),
        bans_before,
        "the ban log is back"
    );
    stand
        .wait_state(&id, "healthy", secs(60))
        .await
        .expect("the first node, which the backup knows, goes on");
}

#[tokio::test]
#[ignore = "needs Docker; run with --ignored"]
async fn traffic_counters_reach_dashboard() {
    let stand = Stand::up().await.expect("the stand comes up");
    let (_one, _id1) = enroll_healthy(&stand, 1, "node-one").await;
    let (two, _id2) = enroll_healthy(&stand, 2, "node-two").await;
    let ip1 = stand.node(1).ip().await.unwrap();

    let total = |stats: &Value| stats["kpi"]["traffic_total_bytes"].as_i64().unwrap_or(0);
    let baseline = total(&stand.stats_summary().await.unwrap());

    // The first reading the agent reports is only a baseline; traffic counts from the next one.
    // The client is a hand-made FakeTLS session (the engine speaks only MTProto), repeated
    // until the dashboard has counted something.
    let started = Instant::now();
    let mut sent = 0;
    let mut seen = baseline;
    while started.elapsed() < secs(120) && seen <= baseline {
        sent += two
            .send_proxy_traffic(&ip1, 443, PROXY_USER_SECRET, TLS_DOMAIN, 10_000)
            .await
            .expect("the session is sent");
        let waited: Result<i64, _> = eventually("the counters move", secs(10), || async {
            match stand.stats_summary().await {
                Ok(s) if total(&s) > baseline => Ok(total(&s)),
                Ok(s) => Err(format!("{}", total(&s))),
                Err(e) => Err(e.to_string()),
            }
        })
        .await;
        if let Ok(now) = waited {
            seen = now;
        }
    }
    assert!(
        seen > baseline,
        "kpi.traffic_total_bytes stayed at {baseline} after {sent} bytes were sent through node one"
    );
}

#[tokio::test]
#[ignore = "needs Docker and the internet; run with --ignored"]
async fn registrar_installer_on_fresh_ubuntu() {
    let stand = Stand::up().await.expect("the stand comes up");
    stand
        .start_fresh_ubuntu()
        .await
        .expect("the bare Ubuntu starts");
    let run = stand
        .fresh_exec(
            "sh /opt/deploy/install-registrar.sh --domain pool.example.test \
             --binary /opt/mtpool-registrar",
        )
        .await
        .unwrap();
    // The output holds the admin token; it is checked for its shape, never shown.
    assert!(
        run.success,
        "the installer works:\n{}",
        redact_token(&run.output)
    );
    assert!(
        run.output.contains("Panel: https://pool.example.test/"),
        "it prints the panel address:\n{}",
        redact_token(&run.output)
    );
    let token_lines = run
        .output
        .lines()
        .filter(|l| l.starts_with("Admin token (shown once, keep it): ") && l.len() > 40)
        .count();
    assert_eq!(token_lines, 1, "the token is printed once");

    // The installer writes the default release repository, starts the path unit that applies
    // updates and the service itself, without a request: its workers run from the start and
    // after a reboot.
    let updates = stand
        .fresh_exec(
            "grep -c '^MTPOOL_RELEASE_REPO=SkyDeaD/MTPooL$' /etc/mtpool/registrar.env; \
             systemctl is-active mtpool-registrar-update.path; \
             stat -c '%U %a' /var/lib/mtpool-update /var/lib/mtpool /var/lib/mtpool/releases; \
             systemctl is-active mtpool-registrar.service; \
             systemctl is-enabled mtpool-registrar.service",
        )
        .await
        .unwrap();
    assert_eq!(
        updates.output.trim(),
        "1\nactive\nroot 755\nmtpool 755\nmtpool 755\nactive\nenabled",
        "the installer sets up self-update and starts the service:\n{}",
        updates.output
    );
    stand
        .point_fresh_registrar_at_fakes()
        .await
        .expect("the environment is edited");

    let states = stand
        .fresh_exec(
            "systemctl is-active mtpool-registrar.socket mtpool-backup.timer caddy; \
             curl -fsS -o /dev/null -w '%{http_code}' http://127.0.0.1:8080/readyz",
        )
        .await
        .unwrap();
    assert!(
        states.success,
        "units are active and the registrar answers:\n{}",
        states.output
    );
    assert!(states.output.ends_with("200"), "{}", states.output);
    // Nobody but the service reads the database: the file and its -wal are 0600.
    let private = stand
        .fresh_exec("stat -c '%a %U' /var/lib/mtpool/registrar.db /var/lib/mtpool/registrar.db-wal")
        .await
        .unwrap();
    assert_eq!(
        private.output.trim(),
        "600 mtpool\n600 mtpool",
        "{}",
        private.output
    );
    let user = stand
        .fresh_exec("systemctl show mtpool-registrar -p User --value; stat -c %U /var/lib/mtpool")
        .await
        .unwrap();
    assert_eq!(user.output.trim(), "mtpool\nmtpool", "{}", user.output);

    // Socket activation: connections wait in the socket's queue while the service restarts, so
    // no request is refused (slow answers are fine).
    let restart = stand
        .fresh_exec(
            "systemctl restart mtpool-registrar.service & \
             i=0; while [ $i -lt 150 ]; do \
               curl -sS -o /dev/null -w '%{http_code}\\n' --max-time 60 http://127.0.0.1:8080/readyz \
                 2>&1 || echo REFUSED; \
               i=$((i+1)); sleep 0.05; done; wait",
        )
        .await
        .unwrap();
    let answers: Vec<&str> = restart.output.lines().collect();
    assert_eq!(answers.len(), 150, "{}", restart.output);
    assert!(
        answers.iter().all(|a| *a == "200"),
        "a request was refused or failed while the service restarted:\n{}",
        answers
            .iter()
            .filter(|a| **a != "200")
            .copied()
            .collect::<Vec<_>>()
            .join("\n")
    );

    // Running the installer again is an upgrade: no new token, the old one still works, and
    // the service runs the new binary.
    let token = run
        .output
        .lines()
        .find_map(|l| l.strip_prefix("Admin token (shown once, keep it): "))
        .expect("the first run printed a token")
        .to_owned();
    let exe_sum =
        "sha256sum /proc/$(systemctl show -p MainPID --value mtpool-registrar)/exe | cut -d' ' -f1";
    let before = stand.fresh_exec(exe_sum).await.unwrap().output;
    stand
        .fresh_exec("cp /opt/mtpool-registrar /opt/mtpool-registrar-v2 && printf x >> /opt/mtpool-registrar-v2")
        .await
        .unwrap();
    let listing = stand
        .fresh_exec("ls -la /var/lib/mtpool; grep -c . /etc/mtpool/registrar.env")
        .await
        .unwrap()
        .output;
    let again = stand
        .fresh_exec(
            "sh /opt/deploy/install-registrar.sh --domain pool.example.test \
             --binary /opt/mtpool-registrar-v2",
        )
        .await
        .unwrap();
    assert!(
        again.success,
        "the upgrade works:\n{}",
        redact_token(&again.output)
    );
    assert!(
        !again.output.contains("Admin token (shown once") && !again.output.contains(&token),
        "an upgrade prints no token:\n{}\n{listing}",
        redact_token(&again.output)
    );
    assert!(
        again.output.contains("unchanged") && again.output.contains("reset-token"),
        "it says the token is unchanged and how to reset it:\n{}",
        again.output
    );
    let login = stand
        .fresh_exec_env(
            "curl -s -o /dev/null -w '%{http_code}' -H \"Authorization: Bearer $T\" \
             http://127.0.0.1:8080/api/v1/admin/settings",
            &[("T", &token)],
        )
        .await
        .unwrap();
    assert_eq!(login.output.trim(), "200", "the first token still logs in");
    let after = stand.fresh_exec(exe_sum).await.unwrap().output;
    let expected = stand
        .fresh_exec("sha256sum /opt/mtpool-registrar-v2 | cut -d' ' -f1")
        .await
        .unwrap()
        .output;
    assert_ne!(before, after, "the service was restarted");
    assert_eq!(after, expected, "the service runs the new binary");

    // A restore run as root hands the database back to the user that serves it.
    let restore = stand
        .fresh_exec(
            "set -e; runuser -u mtpool -- /usr/local/bin/mtpool-registrar backup --to /var/backups/mtpool/r.db; \
             systemctl stop mtpool-registrar.socket mtpool-registrar.service; \
             MTPOOL_DB_PATH=/var/lib/mtpool/registrar.db /usr/local/bin/mtpool-registrar restore \
               --from /var/backups/mtpool/r.db; \
             stat -c '%U' /var/lib/mtpool/registrar.db /var/lib/mtpool/registrar.db.lock; \
             systemctl start mtpool-registrar.socket; \
             curl -fsS --max-time 30 -o /dev/null -w '%{http_code}' http://127.0.0.1:8080/readyz",
        )
        .await
        .unwrap();
    assert!(restore.success, "the restore works:\n{}", restore.output);
    assert!(
        restore.output.contains("mtpool\nmtpool\n") && restore.output.ends_with("200"),
        "the database and its lock belong to mtpool and the registrar serves:\n{}",
        restore.output
    );
}

fn redact_token(output: &str) -> String {
    output
        .lines()
        .map(|l| {
            if l.starts_with("Admin token") {
                "Admin token (shown once, keep it): <hidden>"
            } else {
                l
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

const UNIT_FILES: [(&str, &str); 10] = [
    (
        "mtpool-agent.service",
        include_str!("../../../deploy/systemd/mtpool-agent.service"),
    ),
    (
        "mtpool-agent-rollback.service",
        include_str!("../../../deploy/systemd/mtpool-agent-rollback.service"),
    ),
    (
        "telemt.service",
        include_str!("../../../deploy/systemd/telemt.service"),
    ),
    (
        "mtpool-registrar.service",
        include_str!("../../../deploy/systemd/mtpool-registrar.service"),
    ),
    (
        "mtpool-registrar.socket",
        include_str!("../../../deploy/systemd/mtpool-registrar.socket"),
    ),
    (
        "mtpool-backup.service",
        include_str!("../../../deploy/systemd/mtpool-backup.service"),
    ),
    (
        "mtpool-backup.timer",
        include_str!("../../../deploy/systemd/mtpool-backup.timer"),
    ),
    (
        "mtpool-registrar-update.service",
        include_str!("../../../deploy/systemd/mtpool-registrar-update.service"),
    ),
    (
        "mtpool-registrar-update.path",
        include_str!("../../../deploy/systemd/mtpool-registrar-update.path"),
    ),
    (
        "mtpool-registrar-rollback.service",
        include_str!("../../../deploy/systemd/mtpool-registrar-rollback.service"),
    ),
];

#[tokio::test]
#[ignore = "needs Docker; run with --ignored"]
async fn units_pass_systemd_analyze_verify() {
    let stand = Stand::up().await.expect("the stand comes up");
    let node = stand.node(1);
    // What the units name must exist for `verify` to look at the rest: stand-ins for the
    // registrar's binary and its user.
    node.exec(
        "mkdir -p /tmp/units && install -m 0755 /bin/true /usr/local/bin/mtpool-registrar \
         && install -m 0755 /bin/true /usr/local/bin/mtpool-registrar.prev \
         && install -m 0755 /bin/true /usr/local/bin/mtpool-agent.prev \
         && (id mtpool >/dev/null 2>&1 || useradd --system --no-create-home mtpool)",
    )
    .await
    .unwrap();
    for (name, text) in UNIT_FILES {
        node.try_exec(
            &format!("printf '%s\\n' \"$UNIT\" > /tmp/units/{name}"),
            &[("UNIT", text)],
        )
        .await
        .unwrap();
    }
    let verify = node
        .try_exec(
            "cd /tmp/units && systemd-analyze verify ./*.service ./*.socket ./*.timer ./*.path 2>&1",
            &[],
        )
        .await
        .unwrap();
    assert!(
        verify.success && verify.output.trim().is_empty(),
        "systemd-analyze verify has something to say:\n{}",
        verify.output
    );
}

/// The fresh machine with the installer run (no Caddy), pointed at the GitHub stand-in, and
/// its registrar started. Returns the target of its builds, the sha256 of the build it runs and
/// the build itself.
async fn fresh_registrar_with_updates(stand: &Stand) -> (String, String, Vec<u8>) {
    stand
        .start_fresh_ubuntu()
        .await
        .expect("the bare Ubuntu starts");
    let install = stand
        .fresh_exec(
            "sh /opt/deploy/install-registrar.sh --domain pool.example.test --no-caddy \
             --binary /opt/mtpool-registrar",
        )
        .await
        .unwrap();
    assert!(
        install.success,
        "the installer works:\n{}",
        redact_token(&install.output)
    );
    stand
        .point_fresh_registrar_at_fakes()
        .await
        .expect("the environment is edited");
    let ready = stand
        .fresh_exec(
            "curl -fsS --max-time 30 -o /dev/null -w '%{http_code}' http://127.0.0.1:8080/readyz",
        )
        .await
        .unwrap();
    assert_eq!(ready.output.trim(), "200", "the registrar serves");
    let target = stand.fresh_target().await.expect("the target");
    let sum = installed_sum(stand).await;
    let build = std::fs::read(stand.fetch_registrar_binary().await.expect("the build"))
        .expect("the build is readable");
    (target, sum, build)
}

/// The sha256 of the binary that is installed as the registrar.
async fn installed_sum(stand: &Stand) -> String {
    stand
        .fresh_exec("sha256sum /usr/local/bin/mtpool-registrar | cut -d' ' -f1")
        .await
        .unwrap()
        .output
        .trim()
        .to_owned()
}

async fn fresh_text(stand: &Stand, command: &str) -> String {
    stand.fresh_exec(command).await.unwrap().output
}

#[tokio::test]
#[ignore = "needs Docker and the internet; run with --ignored"]
async fn registrar_updates_and_rolls_back_when_readyz_fails_60s() {
    let stand = Stand::up().await.expect("the stand comes up");
    let key = ensure_test_key().expect("the stand's key");
    let (target, original, build) = fresh_registrar_with_updates(&stand).await;

    // A good release: the same build with a byte added, so it is another file that runs alike.
    let mut good = build.clone();
    good.push(b'x');
    stand
        .publish_github_release(&registrar_release("9.9.9", &target, &good, &key).unwrap())
        .await
        .expect("the release is published");
    eventually("the update is installed", secs(180), || async {
        let installed = fresh_text(&stand, "cat /var/lib/mtpool-update/installed 2>&1").await;
        if installed.trim() == "9.9.9" {
            Ok(())
        } else {
            Err(installed)
        }
    })
    .await
    .unwrap();
    let updated = installed_sum(&stand).await;
    assert_ne!(updated, original, "the binary was replaced");
    let previous = fresh_text(
        &stand,
        "sha256sum /usr/local/bin/mtpool-registrar.prev | cut -d' ' -f1",
    )
    .await;
    assert_eq!(
        previous.trim(),
        original,
        "the previous binary is kept beside it"
    );
    let running = fresh_text(
        &stand,
        "sha256sum /proc/$(systemctl show -p MainPID --value mtpool-registrar)/exe | cut -d' ' -f1",
    )
    .await;
    assert_eq!(running.trim(), updated, "the service runs the new binary");
    let state = fresh_text(
        &stand,
        "curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:8080/readyz; \
         ls /var/lib/mtpool-update; ls /var/lib/mtpool/releases",
    )
    .await;
    assert!(state.starts_with("200"), "{state}");
    assert!(
        !state.contains("probation"),
        "the probation is over:\n{state}"
    );
    assert!(state.contains("manifest.json.minisig"), "{state}");
    let journal = fresh_text(
        &stand,
        "journalctl -u mtpool-registrar --no-pager | grep -c update.published",
    )
    .await;
    assert!(
        journal.trim().parse::<u32>().unwrap_or(0) >= 1,
        "the admin can see it: {journal}"
    );

    // A release whose binary never gets ready: it answers `version` and then only sleeps. The
    // socket keeps connections queued, so `/readyz` hangs for the whole watch.
    let bad = b"#!/bin/sh\n[ \"$1\" = version ] && exec /usr/local/bin/mtpool-registrar version\nexec sleep infinity\n".to_vec();
    let started = Instant::now();
    stand
        .publish_github_release(&registrar_release("9.9.10", &target, &bad, &key).unwrap())
        .await
        .expect("the release is published");
    eventually("the bad release is rejected", secs(240), || async {
        let rejected = fresh_text(&stand, "cat /var/lib/mtpool-update/rejected 2>&1").await;
        if rejected.contains("9.9.10") {
            Ok(())
        } else {
            Err(rejected)
        }
    })
    .await
    .unwrap();
    let took = started.elapsed();
    assert!(
        took >= secs(55),
        "the new binary was watched for about a minute, not {took:?}"
    );
    let restored = eventually("the good build is back and serves", secs(120), || async {
        let sum = installed_sum(&stand).await;
        let code = fresh_text(
            &stand,
            "curl -s --max-time 10 -o /dev/null -w '%{http_code}' http://127.0.0.1:8080/readyz",
        )
        .await;
        if sum == updated && code.trim() == "200" {
            Ok(())
        } else {
            Err(format!("{sum} {code}"))
        }
    })
    .await;
    restored.unwrap();
    let rollback = fresh_text(
        &stand,
        "journalctl -u mtpool-registrar-rollback -u mtpool-registrar-update --no-pager",
    )
    .await;
    assert!(
        rollback.contains("rolled back"),
        "the rollback is in the journal:\n{rollback}"
    );
    let installed = fresh_text(
        &stand,
        "cat /var/lib/mtpool-update/installed; ls /var/lib/mtpool-update",
    )
    .await;
    assert!(installed.starts_with("9.9.9"), "{installed}");
    assert!(!installed.contains("probation"), "{installed}");

    // The rolled-back release is not tried again.
    let downloads_then = stand.github_downloads().await.unwrap().len();
    tokio::time::sleep(secs(15)).await;
    assert_eq!(
        stand.github_downloads().await.unwrap().len(),
        downloads_then,
        "nothing was downloaded again"
    );
    assert_eq!(
        installed_sum(&stand).await,
        updated,
        "and nothing was applied again"
    );
}

#[tokio::test]
#[ignore = "needs Docker and the internet; run with --ignored"]
async fn bad_signature_release_rejected() {
    let stand = Stand::up().await.expect("the stand comes up");
    let foreign = TestKey::generate().expect("a key the build does not know");
    let (target, original, build) = fresh_registrar_with_updates(&stand).await;
    let mut other = build;
    other.push(b'x');
    stand
        .publish_github_release(&registrar_release("9.9.9", &target, &other, &foreign).unwrap())
        .await
        .expect("the release is published");
    eventually("the release is looked at and refused", secs(60), || async {
        let log = fresh_text(
            &stand,
            "journalctl -u mtpool-registrar --no-pager | grep update.rejected",
        )
        .await;
        if log.trim().is_empty() {
            Err("not yet".to_owned())
        } else {
            Ok(log)
        }
    })
    .await
    .unwrap();
    // A few more polls: nothing changes and nothing is fetched twice.
    tokio::time::sleep(secs(8)).await;
    let downloads = stand.github_downloads().await.unwrap();
    assert!(
        downloads.iter().all(|d| d.starts_with("manifest.json")) && !downloads.is_empty(),
        "only the manifest and its signature were fetched: {downloads:?}"
    );
    let files = fresh_text(
        &stand,
        "ls -A /var/lib/mtpool/releases; ls -A /var/lib/mtpool-update; ls /usr/local/bin",
    )
    .await;
    assert!(
        !files.contains("mtpool-registrar-"),
        "no artifact was published:\n{files}"
    );
    assert!(
        !files.contains("installed") && !files.contains("probation"),
        "{files}"
    );
    assert!(!files.contains("mtpool-registrar.prev"), "{files}");
    assert_eq!(
        installed_sum(&stand).await,
        original,
        "the binary is untouched"
    );
    let ready = fresh_text(
        &stand,
        "curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:8080/readyz",
    )
    .await;
    assert_eq!(ready.trim(), "200");
}
