//! The operator's scenarios: installing a node, backing the registrar up and restoring it, and
//! the traffic counters. Needs Docker (and, for the installers, the internet: they install
//! `minisign` and the engine from the distribution and from GitHub), so all are ignored by
//! default: `cargo test -p mtpool-e2e --test operations -- --ignored`.

mod common;

use std::time::Instant;

use common::{enroll_healthy, secs, settings};
use mtpool_e2e::{
    InstallOptions, PROXY_USER_SECRET, ReleaseSpec, Stand, TLS_DOMAIN, TestKey, Token,
    ensure_test_key, eventually,
};
use serde_json::Value;

/// Offers the agent of node 1's image as a release signed by the stand's key.
async fn offer_release(stand: &Stand, key: &TestKey) {
    let node = stand.node(1);
    let binary = node.fetch_agent_binary().await.expect("the agent build");
    let target = node.target().await.expect("the target");
    stand
        .sign_release(&ReleaseSpec {
            version: "9.9.9",
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
        run.output.to_lowercase().contains("signature"),
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
        run.output
            .contains("engine listens on 8443, the pool expects 443"),
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

const UNIT_FILES: [(&str, &str); 6] = [
    (
        "mtpool-agent.service",
        include_str!("../../../deploy/systemd/mtpool-agent.service"),
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
            "cd /tmp/units && systemd-analyze verify ./*.service ./*.socket ./*.timer 2>&1",
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
