//! The end-to-end scenarios of the spec (section 12), each on a stand of its own. They need
//! Docker, so they are ignored by default:
//! `cargo test -p mtpool-e2e --test scenarios -- --ignored --nocapture`.

mod common;

use std::time::Instant;

use common::{enroll_healthy, last_round, secs, set_sensors, settings, vote_of, wait_calls};
use mtpool_e2e::{DOMAIN, Stand, StandOptions, eventually};
use serde_json::json;

/// One `Blocked` vote leaves the node in quarantine; the second one bans it.
#[tokio::test]
#[ignore = "needs Docker; run with --ignored"]
async fn one_vote_no_ban_two_votes_ban() {
    let stand = Stand::up().await.expect("the stand comes up");
    let (_node, id) = enroll_healthy(&stand, 1, "node-one").await;

    // Globalping alone says blocked: cheburcheck and RedFlag answer clear.
    set_sensors(&stand, &[("globalping", "blocked")]).await;
    stand
        .wait_state_in(&id, &["quarantine"], secs(60))
        .await
        .expect("a failed Globalping check puts the node in quarantine");
    let round = eventually("a round with one Blocked vote", secs(60), || async {
        match last_round(&stand, &id).await {
            Some(round) => Ok(round),
            None => Err("no round yet".to_owned()),
        }
    })
    .await
    .expect("a round runs");
    let (verdict, votes) = round;
    assert_eq!(
        vote_of(&votes, "globalping").as_deref(),
        Some("blocked"),
        "{votes:?}"
    );
    assert_eq!(verdict, "undecided", "{votes:?}");
    // A few more rounds change nothing.
    tokio::time::sleep(secs(8)).await;
    assert_eq!(
        stand.node_state(&id).await.unwrap().as_deref(),
        Some("quarantine")
    );
    assert!(
        stand.bans().await.unwrap().is_empty(),
        "one vote banned the node"
    );

    // The second independent sensor agrees.
    set_sensors(&stand, &[("cheburcheck", "blocked")]).await;
    wait_banned(&stand, &id, secs(60)).await;
    assert_eq!(
        stand.node_state(&id).await.unwrap(),
        None,
        "a banned node has no record"
    );
    let bans = stand.bans().await.unwrap();
    assert_eq!(bans.len(), 1, "{bans:?}");
    assert_eq!(bans[0]["node_id"], json!(id), "{bans:?}");
    let blocked = bans[0]["votes"].to_string().matches("blocked").count();
    assert!(
        blocked >= 2,
        "the ban records its votes: {}",
        bans[0]["votes"]
    );
}

/// Every sensor says nothing useful: no ban, and the node is pruned when its quarantine runs out.
#[tokio::test]
#[ignore = "needs Docker; run with --ignored"]
async fn all_sensors_down_no_ban_then_pruned() {
    let stand = Stand::up().await.expect("the stand comes up");
    let (_node, id) = enroll_healthy(&stand, 1, "node-one").await;
    settings(
        &stand,
        // The agent's report must not vote for Globalping in the rounds: they are about sensors
        // that are down.
        json!({
            "quarantine_max_sec": 120,
            "globalping": {"vote_report_max_age_sec": 1},
        }),
    )
    .await;

    // A failed check makes the node suspected; a stale report alone would not run sensors.
    set_sensors(
        &stand,
        &[
            ("globalping", "blocked"),
            ("cheburcheck", "unavailable"),
            ("redflag", "unauthorized"),
        ],
    )
    .await;
    stand
        .wait_state_in(&id, &["quarantine"], secs(90))
        .await
        .expect("the node with a failed Globalping check goes to quarantine");
    set_sensors(&stand, &[("globalping", "timeout")]).await;
    // A round must happen in which every one of the three sensors abstained. The first rounds
    // may still have the Globalping vote of the failed check: one vote, no ban.
    let (verdict, votes) = eventually(
        "a round in which every sensor abstains",
        secs(90),
        || async {
            match last_round(&stand, &id).await {
                Some((verdict, votes))
                    if ["globalping", "cheburcheck", "red_flag"]
                        .iter()
                        .all(|s| vote_of(&votes, s).as_deref() == Some("abstain")) =>
                {
                    Ok((verdict, votes))
                }
                other => Err(format!("{other:?}")),
            }
        },
    )
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(verdict, "undecided", "{votes:?}");
    for sensor in ["globalping", "cheburcheck", "red_flag"] {
        assert_eq!(
            vote_of(&votes, sensor).as_deref(),
            Some("abstain"),
            "{sensor} voted although it was down: {votes:?}"
        );
    }
    assert!(
        stand.bans().await.unwrap().is_empty(),
        "a ban without any vote"
    );
    stand
        .wait_state_in(&id, &["pruned"], secs(150))
        .await
        .expect("the node leaves quarantine and is pruned, not banned");
    assert!(stand.bans().await.unwrap().is_empty());
}

/// The quick `blocked: true` flag of cheburcheck is not a vote when its probes come back clean.
#[tokio::test]
#[ignore = "needs Docker; run with --ignored"]
async fn cheburcheck_fast_flag_alone_no_vote() {
    let stand = Stand::up().await.expect("the stand comes up");
    let (_node, id) = enroll_healthy(&stand, 1, "node-one").await;

    // Sensors are asked only about a node in quarantine, so Globalping is made to fail. The
    // stand-in cheburcheck answers its default: the fast flag says blocked, the probes say clear.
    let before = stand.sensor_calls("cheburcheck").await.unwrap();
    set_sensors(&stand, &[("globalping", "blocked")]).await;
    stand
        .wait_state_in(&id, &["quarantine"], secs(60))
        .await
        .expect("the node goes to quarantine");
    wait_calls(&stand, "cheburcheck", before, 3, 60).await;

    let (verdict, votes) = eventually("a round with a cheburcheck vote", secs(30), || async {
        match last_round(&stand, &id).await {
            Some((v, votes)) if vote_of(&votes, "cheburcheck").is_some() => Ok((v, votes)),
            other => Err(format!("{other:?}")),
        }
    })
    .await
    .expect("a round");
    assert_eq!(
        vote_of(&votes, "cheburcheck").as_deref(),
        Some("clear"),
        "the fast flag alone gave a vote: {votes:?}"
    );
    assert_eq!(
        vote_of(&votes, "globalping").as_deref(),
        Some("blocked"),
        "{votes:?}"
    );
    assert_eq!(verdict, "undecided", "{votes:?}");
    assert!(stand.bans().await.unwrap().is_empty());
    assert_eq!(
        stand.node_state(&id).await.unwrap().as_deref(),
        Some("quarantine")
    );
}

/// A Globalping answer of HTTP 403 over a finished TLS handshake is a success.
#[tokio::test]
#[ignore = "needs Docker; run with --ignored"]
async fn gp_403_with_tls_healthy() {
    let stand = Stand::up().await.expect("the stand comes up");
    let (_node, id) = enroll_healthy(&stand, 1, "node-one").await;

    // The stand-in's `ok` answer is the recorded 403 with TLS: the agent keeps measuring and the
    // node stays healthy.
    let before = stand.sensor_calls("globalping").await.unwrap();
    wait_calls(&stand, "globalping", before, 2, 60).await;
    assert_eq!(
        stand.node_state(&id).await.unwrap().as_deref(),
        Some("healthy")
    );

    // And it is what lets a node out of quarantine: a failed check, then the 403 again.
    set_sensors(&stand, &[("globalping", "blocked")]).await;
    stand
        .wait_state_in(&id, &["quarantine"], secs(60))
        .await
        .expect("a failed check quarantines the node");
    set_sensors(&stand, &[("globalping", "ok")]).await;
    stand
        .wait_state_in(&id, &["healthy"], secs(90))
        .await
        .expect("the 403 with TLS brings the node back");
    let (_, votes) = last_round(&stand, &id)
        .await
        .expect("the recovery was decided by rounds");
    assert_eq!(
        vote_of(&votes, "globalping").as_deref(),
        Some("clear"),
        "{votes:?}"
    );
    assert!(stand.bans().await.unwrap().is_empty());
}

/// An engine that is stopped makes a dead node, never a banned one.
#[tokio::test]
#[ignore = "needs Docker; run with --ignored"]
async fn stopped_engine_dead_not_banned() {
    let stand = Stand::up().await.expect("the stand comes up");
    let (node, id) = enroll_healthy(&stand, 1, "node-one").await;

    node.stop_engine().await.expect("the engine stops");
    stand
        .wait_state_in(&id, &["dead"], secs(120))
        .await
        .expect("the node leaves the pool");
    assert!(stand.bans().await.unwrap().is_empty());
    assert!(
        stand
            .events(Some("node_banned"), Some(&id))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        stand
            .events(Some("node_dead"), Some(&id))
            .await
            .unwrap()
            .len(),
        1
    );
}

/// With Cloudflare down, masters are still chosen and the DNS keeps what it has; when
/// Cloudflare is back the new record is made first and the old one removed after it.
#[tokio::test]
#[ignore = "needs Docker; run with --ignored"]
async fn cloudflare_down_selection_continues_dns_recovers() {
    let stand = Stand::up().await.expect("the stand comes up");
    let (node1, id1) = enroll_healthy(&stand, 1, "node-one").await;
    stand
        .wait_master(DOMAIN, &id1, secs(60))
        .await
        .expect("node one is the master");
    let ip1 = node1.ip().await.unwrap();
    stand
        .wait_dns_record(DOMAIN, &ip1, secs(60))
        .await
        .expect("its record");

    set_sensors(&stand, &[("cloudflare", "rate_limited")]).await;
    let (node2, id2) = enroll_healthy(&stand, 2, "node-two").await;
    let ip2 = node2.ip().await.unwrap();
    stand
        .admin
        .post(&format!("/api/v1/admin/nodes/{id1}/retire"), json!({}))
        .await
        .expect("node one is retired");
    stand
        .wait_master(DOMAIN, &id2, secs(60))
        .await
        .expect("selection goes on: node two becomes the master");

    // Cloudflare refuses everything, so the DNS stays as it was.
    let until = Instant::now() + secs(10);
    while Instant::now() < until {
        let state = stand.fakes_state().await.unwrap();
        let dns = state["dns"].to_string();
        assert!(dns.contains(&ip1), "the old record went away early: {dns}");
        assert!(
            !dns.contains(&ip2),
            "the new record appeared while Cloudflare was down: {dns}"
        );
        tokio::time::sleep(secs(1)).await;
    }

    set_sensors(&stand, &[("cloudflare", "ok")]).await;
    stand
        .wait_dns_record(DOMAIN, &ip2, secs(60))
        .await
        .expect("the DNS catches up");
    let log = eventually("the old record is deleted", secs(60), || async {
        let state = stand.fakes_state().await.map_err(|e| e.to_string())?;
        let log = state["dns_log"].as_array().cloned().unwrap_or_default();
        if log
            .iter()
            .any(|e| e["op"] == "delete" && e["content"] == json!(ip1))
        {
            Ok(log)
        } else {
            Err(format!("{log:?}"))
        }
    })
    .await
    .expect("the old record goes");
    let at = |op: &str, ip: &str| {
        log.iter()
            .position(|e| e["op"] == op && e["content"] == json!(ip))
    };
    let created = at("create", &ip2).expect("the new record was created");
    let deleted = at("delete", &ip1).expect("the old record was deleted");
    assert!(
        created < deleted,
        "the old record was deleted before the new one existed: {log:?}"
    );
}

/// The registrar restarts while three nodes report: no node is declared dead, all come back.
#[tokio::test]
#[ignore = "needs Docker; run with --ignored"]
async fn restart_under_load_no_false_deaths() {
    let stand = Stand::up().await.expect("the stand comes up");
    let mut ids = Vec::new();
    for n in 1..=3 {
        let (_node, id) = enroll_healthy(&stand, n, &format!("node-{n}")).await;
        ids.push(id);
    }

    stand
        .restart_registrar()
        .await
        .expect("the registrar restarts");

    // Twice the time a silent node needs to be called dead.
    let until = Instant::now() + secs(60);
    while Instant::now() < until {
        let dead = stand.events(Some("node_dead"), None).await.unwrap();
        assert!(
            dead.is_empty(),
            "a node was declared dead after the restart: {dead:?}"
        );
        tokio::time::sleep(secs(2)).await;
    }
    for id in &ids {
        stand
            .wait_state_in(id, &["healthy"], secs(30))
            .await
            .expect("the state came back: the node is healthy");
    }
}

/// A database that cannot be written to shows in `/readyz`.
#[tokio::test]
#[ignore = "needs Docker; run with --ignored"]
async fn db_write_error_visible_in_readyz() {
    let stand = Stand::up_with(StandOptions {
        small_db: true,
        ..StandOptions::default()
    })
    .await
    .expect("the stand comes up");
    let (_node, _id) = enroll_healthy(&stand, 1, "node-one").await;
    assert_eq!(
        stand.readyz().await.unwrap(),
        200,
        "the registrar is ready before the fault"
    );

    stand
        .break_db_writes()
        .await
        .expect("the database volume fills up");
    let seen = eventually(
        "/readyz says the registrar cannot write",
        secs(60),
        || async {
            match stand.readyz().await {
                Ok(503) => Ok(503),
                Ok(code) => Err(format!("{code}")),
                Err(e) => Err(e.to_string()),
            }
        },
    )
    .await;
    assert!(seen.is_ok(), "{:?}", seen.err());
}

/// A banned address whose node asks again is checked once more: a clean answer lifts the ban, a
/// dirty one keeps it.
#[tokio::test]
#[ignore = "needs Docker; run with --ignored"]
async fn reverify_clean_lifts_dirty_keeps() {
    let stand = Stand::up().await.expect("the stand comes up");
    let (_node, id) = enroll_healthy(&stand, 1, "node-one").await;
    settings(&stand, json!({"reverify_pause_sec": 10})).await;

    // Two sensors say blocked: the node is banned.
    set_sensors(
        &stand,
        &[("globalping", "blocked"), ("cheburcheck", "blocked")],
    )
    .await;
    wait_banned(&stand, &id, secs(90)).await;

    // The address is clean again. The agent lost its token with the record and, the pause over,
    // asks the pool for a place by itself.
    set_sensors(&stand, &[("globalping", "ok"), ("cheburcheck", "ok")]).await;
    eventually("the ban is lifted", secs(90), || async {
        match stand.events(Some("ban_lifted"), None).await {
            Ok(events) if !events.is_empty() => Ok(()),
            Ok(_) => Err("no ban_lifted event yet".to_owned()),
            Err(e) => Err(e.to_string()),
        }
    })
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    let bans = stand.bans().await.unwrap();
    assert_eq!(bans.len(), 1, "{bans:?}");
    assert!(
        !bans[0]["lifted_at"].is_null(),
        "the ban is marked lifted: {bans:?}"
    );
    let new_id = healthy_node_other_than(&stand, &id).await;

    // The same again, but one sensor still says blocked: the ban stays.
    set_sensors(
        &stand,
        &[("globalping", "blocked"), ("cheburcheck", "blocked")],
    )
    .await;
    wait_banned(&stand, &new_id, secs(90)).await;
    set_sensors(&stand, &[("cheburcheck", "ok")]).await;
    eventually("the reverify fails", secs(90), || async {
        match stand.events(Some("node_reverify_failed"), None).await {
            Ok(events) if !events.is_empty() => Ok(()),
            Ok(_) => Err("no node_reverify_failed event yet".to_owned()),
            Err(e) => Err(e.to_string()),
        }
    })
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    let bans = stand.bans().await.unwrap();
    // The failed re-check is written to the ban log as a row of its own (`reverify: true`) that
    // keeps the dirty verdict; the first ban stays the only lifted one. The agent keeps asking
    // after each pause, so there may be more re-checks by now.
    assert!(bans.len() >= 3, "{bans:?}");
    let open: Vec<_> = bans.iter().filter(|b| b["lifted_at"].is_null()).collect();
    assert_eq!(
        open.len(),
        bans.len() - 1,
        "the second ban and the failed re-checks stay: {bans:?}"
    );
    let recheck = open
        .iter()
        .find(|b| b["reverify"] == true)
        .unwrap_or_else(|| panic!("a re-check row is open: {bans:?}"));
    assert!(
        recheck["votes"].as_array().is_some_and(|v| v
            .iter()
            .any(|x| x["sensor"] == "globalping" && x["vote"] == "blocked")),
        "the re-check keeps the blocked vote: {recheck:?}"
    );
    let lifted = stand.events(Some("ban_lifted"), None).await.unwrap();
    assert_eq!(
        lifted.len(),
        1,
        "only the clean reverify lifted a ban: {lifted:?}"
    );
}

/// Waits for the ban of the node. It has no state to wait for: the record goes with the ban.
async fn wait_banned(stand: &Stand, id: &str, within: std::time::Duration) {
    eventually("two Blocked votes ban the node", within, || async {
        match stand.events(Some("node_banned"), Some(id)).await {
            Ok(events) if !events.is_empty() => Ok(()),
            Ok(_) => Err("no node_banned event yet".to_owned()),
            Err(e) => Err(e.to_string()),
        }
    })
    .await
    .unwrap_or_else(|e| panic!("{e}"));
}

/// The id of a healthy node of the pool that is not `old`.
async fn healthy_node_other_than(stand: &Stand, old: &str) -> String {
    eventually("a new healthy node", secs(90), || async {
        match stand.nodes().await {
            Ok(nodes) => nodes
                .iter()
                .find(|n| n["state"] == "healthy" && n["id"] != old)
                .and_then(|n| n["id"].as_str().map(str::to_owned))
                .ok_or_else(|| format!("{nodes:?}")),
            Err(e) => Err(e.to_string()),
        }
    })
    .await
    .unwrap_or_else(|e| panic!("{e}"))
}

/// A node that loses all its packets for a while leaves the healthy state, and comes back when
/// the network does.
#[tokio::test]
#[ignore = "needs Docker; run with --ignored"]
async fn network_loss_via_netem_recovers() {
    let stand = Stand::up().await.expect("the stand comes up");
    let (node, id) = enroll_healthy(&stand, 1, "node-one").await;

    node.netem(100)
        .await
        .expect("netem is available in the node");
    // Twice the heartbeat time-to-live, which is less than the time a node needs to be dead.
    let state = stand
        .wait_state_in(&id, &["quarantine", "dead"], secs(30))
        .await
        .expect("a node that loses every packet is not healthy");
    tokio::time::sleep(secs(10)).await;
    let state_later = stand.node_state(&id).await.unwrap();
    assert!(
        matches!(state_later.as_deref(), Some("quarantine" | "dead")),
        "still down after 10 s: {state_later:?} (was {state})"
    );

    node.netem(0).await.expect("netem is removed again");
    let back = stand.wait_state_in(&id, &["healthy"], secs(120)).await;
    if let Err(e) = back {
        panic!(
            "{e}\nagent log:\n{}",
            node.agent_log().await.unwrap_or_default()
        );
    }
    assert!(
        stand.bans().await.unwrap().is_empty(),
        "packet loss is not a ban"
    );
}
