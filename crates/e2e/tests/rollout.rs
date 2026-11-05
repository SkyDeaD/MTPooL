//! The rollout of an agent release over three nodes: a canary, batches, and a stop when a build
//! does not come up. Needs Docker, so it is ignored by default:
//! `cargo test -p mtpool-e2e --test rollout -- --ignored --nocapture`.

mod common;

use std::path::Path;

use common::{enroll_healthy, run_registrar_build, secs, settings};
use mtpool_e2e::{ReleaseSpec, Stand, TestKey, ensure_test_key, eventually};
use serde_json::{Value, json};

const ROLLOUTS: &str = "/api/v1/admin/rollouts";

async fn rollouts(stand: &Stand) -> Vec<Value> {
    stand
        .admin
        .get(ROLLOUTS)
        .await
        .expect("the rollouts are listed")
        .as_array()
        .cloned()
        .unwrap_or_default()
}

async fn rollout(stand: &Stand, id: i64) -> Value {
    stand
        .admin
        .get(&format!("{ROLLOUTS}/{id}"))
        .await
        .expect("the rollout is shown")
}

/// Waits until the rollout `id` is in `phase`, and returns its view.
async fn wait_phase(stand: &Stand, id: i64, phase: &str, within: u64) -> Value {
    let found = eventually(
        &format!("rollout {id} is {phase}"),
        secs(within),
        || async {
            let view = rollout(stand, id).await;
            if view["phase"] == phase {
                Ok(view)
            } else {
                Err(format!("{}: {}", view["phase"], view["note"]))
            }
        },
    )
    .await;
    match found {
        Ok(view) => view,
        Err(e) => {
            dump_nodes(stand).await;
            panic!("{e}")
        }
    }
}

/// What the agents said, for a failed run to show.
async fn dump_nodes(stand: &Stand) {
    for n in 1..=3 {
        let out = stand
            .node(n)
            .exec(
                "journalctl -u mtpool-agent -u mtpool-agent-rollback --no-pager -n 60 2>&1; \
                 ls -la /var/lib/mtpool/update /usr/local/bin 2>&1",
            )
            .await
            .unwrap_or_default();
        eprintln!("---- node {n}\n{out}");
    }
}

async fn put_release(stand: &Stand, version: &str, build: &Path, key: &TestKey) {
    let node = stand.node(1);
    let target = node.target().await.expect("the target");
    stand
        .sign_release(&ReleaseSpec {
            version,
            agent_binary: build,
            target: &target,
            key,
        })
        .await
        .expect("the release is put on the registrar");
}

/// The version the agent binary of the node says it is.
async fn agent_version(stand: &Stand, n: u32) -> String {
    let out = stand
        .node(n)
        .exec("/usr/local/bin/mtpool-agent version")
        .await
        .expect("the agent answers");
    out.split_whitespace().next().unwrap_or("").to_owned()
}

async fn pid_of(stand: &Stand, n: u32, unit: &str) -> String {
    stand
        .node(n)
        .exec(&format!("systemctl show -p MainPID --value {unit}"))
        .await
        .expect("the unit is known")
        .trim()
        .to_owned()
}

fn number_of(node_id: &str, ids: &[String]) -> u32 {
    ids.iter().position(|i| i == node_id).expect("a known node") as u32 + 1
}

/// The stand's registrar is 0.9.0 (`e2e/images/registrar/Dockerfile`), and 0.9.1 is its next build.
#[tokio::test]
#[ignore = "needs Docker; run with --ignored"]
async fn registrar_rollback_stops_the_rollout_of_its_agent() {
    let stand = Stand::up().await.expect("the stand comes up");
    let key = ensure_test_key().expect("the stand's key");
    let mut ids = Vec::new();
    for n in 1..=3 {
        let (_, id) = enroll_healthy(&stand, n, &format!("node-{n}")).await;
        ids.push(id);
    }
    // The observation outlasts the restart of the registrar: the rollback finds the canary
    // observed and the other nodes untouched.
    settings(
        &stand,
        json!({"rollout": {
            "observe_sec": 45, "batch_interval_sec": 5, "batch_percent": 50,
            "return_sec": 90, "offer_wait_sec": 60
        }}),
    )
    .await;
    let run = stand
        .registrar_exec("cp /usr/local/bin/mtpool-registrar /opt/e2e/registrar-0.9.0")
        .await
        .expect("the old build is kept");
    assert!(run.success, "{}", run.output);
    let build = stand
        .node(1)
        .fetch_file("/opt/e2e/agent-0.9.1")
        .await
        .expect("the 0.9.1 build");

    // The release lands before the registrar is replaced: its agent waits for the registrar.
    put_release(&stand, "0.9.1", &build, &key).await;
    tokio::time::sleep(secs(25)).await;
    assert!(
        rollouts(&stand).await.is_empty(),
        "the old registrar started a rollout of a newer agent"
    );

    // The new registrar takes it out: the canary goes first.
    run_registrar_build(&stand, "0.9.1").await;
    let first = eventually("a rollout starts", secs(60), || async {
        match rollouts(&stand).await.first() {
            Some(r) => Ok(r.clone()),
            None => Err("no rollout".to_owned()),
        }
    })
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    let id = first["id"].as_i64().expect("an id");
    assert_eq!(first["version"], "0.9.1");
    let observing = wait_phase(&stand, id, "observing", 90).await;
    let canary = observing["canary"].as_str().expect("a canary").to_owned();
    let c = number_of(&canary, &ids);
    assert_eq!(
        agent_version(&stand, c).await,
        "0.9.1",
        "the canary runs it"
    );

    // The rollback: the manifest and the open rollout stay, the old build must not go on.
    run_registrar_build(&stand, "0.9.0").await;
    let stopped = rollout(&stand, id).await;
    assert_eq!(stopped["phase"], "cancelled", "{stopped}");
    assert_eq!(stopped["note"]["code"], "registrar_behind", "{stopped}");
    // Longer than the rest of the observation and two batches.
    tokio::time::sleep(secs(60)).await;
    assert_eq!(rollouts(&stand).await.len(), 1, "no new rollout started");
    for n in (1..=3).filter(|n| *n != c) {
        assert_eq!(
            agent_version(&stand, n).await,
            "0.0.0",
            "node {n} untouched"
        );
    }
    assert_eq!(
        agent_version(&stand, c).await,
        "0.9.1",
        "the canary is not moved back"
    );
    stand
        .wait_state(&canary, "healthy", secs(120))
        .await
        .expect("the old registrar works with the newer agent");

    // The registrar reaches that version after all: the agent goes out to everyone.
    run_registrar_build(&stand, "0.9.1").await;
    let again = eventually("a second rollout starts", secs(60), || async {
        rollouts(&stand)
            .await
            .into_iter()
            .find(|r| r["id"] != id)
            .ok_or_else(|| "not yet".to_owned())
    })
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    let second = again["id"].as_i64().expect("an id");
    wait_phase(&stand, second, "done", 240).await;
    for n in 1..=3 {
        assert_eq!(agent_version(&stand, n).await, "0.9.1");
    }
}

#[tokio::test]
#[ignore = "needs Docker; run with --ignored"]
async fn release_rolls_through_canary_and_batches_and_stops_on_failure() {
    let stand = Stand::up().await.expect("the stand comes up");
    let key = ensure_test_key().expect("the stand's key");
    let mut ids = Vec::new();
    for n in 1..=3 {
        let (_, id) = enroll_healthy(&stand, n, &format!("node-{n}")).await;
        ids.push(id);
    }
    settings(
        &stand,
        json!({"rollout": {
            "observe_sec": 12, "batch_interval_sec": 8, "batch_percent": 50,
            "return_sec": 90, "offer_wait_sec": 60
        }}),
    )
    .await;
    let engines: Vec<String> = {
        let mut v = Vec::new();
        for n in 1..=3 {
            v.push(pid_of(&stand, n, "telemt").await);
        }
        v
    };
    let build = stand
        .node(1)
        .fetch_file("/opt/e2e/agent-0.0.1")
        .await
        .expect("the 0.0.1 build");

    // A release signed with a key the pool does not know is never offered.
    let foreign = TestKey::generate().expect("a foreign key");
    put_release(&stand, "0.0.1", &build, &foreign).await;
    tokio::time::sleep(secs(25)).await;
    assert!(
        rollouts(&stand).await.is_empty(),
        "a badly signed release started a rollout"
    );
    assert_eq!(agent_version(&stand, 1).await, "0.0.0");

    // The good release: the canary goes first, then the batches.
    put_release(&stand, "0.0.1", &build, &key).await;
    let started = eventually("a rollout starts", secs(60), || async {
        match rollouts(&stand).await.first() {
            Some(r) => Ok(r.clone()),
            None => Err("no rollout".to_owned()),
        }
    })
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    let id = started["id"].as_i64().expect("an id");
    assert_eq!(started["version"], "0.0.1");
    let observing = wait_phase(&stand, id, "observing", 90).await;
    let canary = observing["canary"].as_str().expect("a canary").to_owned();
    let c = number_of(&canary, &ids);
    assert_eq!(
        agent_version(&stand, c).await,
        "0.0.1",
        "the canary runs it"
    );
    for n in (1..=3).filter(|n| *n != c) {
        assert_eq!(agent_version(&stand, n).await, "0.0.0", "node {n} waits");
    }
    wait_phase(&stand, id, "done", 120).await;
    for n in 1..=3 {
        assert_eq!(agent_version(&stand, n).await, "0.0.1");
        assert_eq!(
            pid_of(&stand, n, "telemt").await,
            engines[n as usize - 1],
            "the engine of node {n} was not restarted"
        );
    }
    let view = rollout(&stand, id).await;
    assert_eq!(view["counts"]["updated"], 3, "{view}");

    // A build that never comes up: it answers `version` and exits when run.
    let version_line = stand
        .node(1)
        .exec("/usr/local/bin/mtpool-agent version")
        .await
        .unwrap();
    let key_id = version_line
        .split("signing key ")
        .nth(1)
        .and_then(|s| s.split(')').next())
        .expect("the agent says its signing key")
        .to_owned();
    let dir = std::env::temp_dir().join(format!("mtpool-e2e-bad-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bad = dir.join("agent-bad");
    std::fs::write(
        &bad,
        format!(
            "#!/bin/sh\nif [ \"$1\" = version ]; then echo '0.0.2 (signing key {key_id})'; exit 0; fi\nexit 1\n"
        ),
    )
    .unwrap();
    put_release(&stand, "0.0.2", &bad, &key).await;
    let second = eventually("the second rollout starts", secs(60), || async {
        match rollouts(&stand)
            .await
            .iter()
            .find(|r| r["version"] == "0.0.2")
        {
            Some(r) => Ok(r.clone()),
            None => Err("not yet".to_owned()),
        }
    })
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    let id2 = second["id"].as_i64().unwrap();
    let paused = wait_phase(&stand, id2, "paused", 240).await;
    let note = &paused["note"];
    assert!(
        note["code"].as_str().is_some_and(|c| !c.is_empty())
            && note["text"].as_str().is_some_and(|t| !t.is_empty()),
        "the pause says why: {paused}"
    );
    assert_eq!(paused["can_resume"], true);
    let failed = paused["canary"].as_str().expect("a canary").to_owned();
    let f = number_of(&failed, &ids);
    // The canary is back on the build it had; the other nodes were never touched.
    eventually("the canary runs the old build again", secs(120), || async {
        let version = agent_version(&stand, f).await;
        let state = stand.node_state(&failed).await.unwrap_or_default();
        if version == "0.0.1" && state.as_deref() == Some("healthy") {
            Ok(())
        } else {
            Err(format!("{version} {state:?}"))
        }
    })
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    let rejected = stand
        .node(f)
        .exec("cat /var/lib/mtpool/update/rejected 2>&1 || true")
        .await
        .unwrap();
    assert!(
        rejected.contains("0.0.2"),
        "the agent remembers: {rejected}"
    );
    assert_eq!(
        pid_of(&stand, f, "telemt").await,
        engines[f as usize - 1],
        "the engine kept running through the failed update"
    );
    for n in (1..=3).filter(|n| *n != f) {
        assert_eq!(
            agent_version(&stand, n).await,
            "0.0.1",
            "node {n} untouched"
        );
    }

    // "Continue" is an audited admin action; the build is still bad, so the rollout stops again.
    let resumed = stand
        .admin
        .post(&format!("{ROLLOUTS}/{id2}/resume"), json!({}))
        .await
        .expect("the rollout resumes");
    assert_ne!(resumed["phase"], "paused", "{resumed}");
    wait_phase(&stand, id2, "paused", 240).await;
    let cancelled = stand
        .admin
        .post(&format!("{ROLLOUTS}/{id2}/cancel"), json!({}))
        .await
        .expect("the rollout is cancelled");
    assert_eq!(cancelled["phase"], "cancelled", "{cancelled}");
    for n in 1..=3 {
        assert_eq!(agent_version(&stand, n).await, "0.0.1");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
