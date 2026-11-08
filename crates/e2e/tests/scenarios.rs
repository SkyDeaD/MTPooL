//! The end-to-end scenarios of the spec (section 12), each on a stand of its own. They need
//! Docker, so they are ignored by default:
//! `cargo test -p mtpool-e2e --test scenarios -- --ignored --nocapture`.

mod common;

use std::time::Instant;

use common::{
    check_host_fetches, check_host_limited, check_host_starts, enroll_healthy, last_round,
    last_round_at, secs, set_sensors, settings, vote_in, vote_of, wait_calls, wait_event,
    wait_flag, wait_round, wait_round_after,
};
use mtpool_e2e::{DOMAIN, Node, Stand, StandOptions, eventually};
use serde_json::json;

/// A node whose Globalping check fails while RedFlag and check-host see it fine goes through
/// quarantine and is brought back by one clean round, with a note that Globalping disagrees.
async fn disputed_node(stand: &Stand) -> (Node<'_>, String) {
    // The check the agent makes when it enrols is the one a round can use, so check-host is set first.
    set_sensors(stand, &[("check_host", "ok")]).await;
    let (node, id) = enroll_healthy(stand, 1, "node-one").await;
    let before = check_host_starts(stand).await.len();
    assert!(
        before >= 1,
        "the agent made no check-host check at enrolment"
    );

    set_sensors(stand, &[("globalping", "blocked")]).await;
    // The flag is already true on a healthy node whose check failed, before a round has moved it
    // to quarantine. So the order is read from the events: quarantine first, then the way back,
    // and only then the flag is looked at.
    let quarantined = wait_event(stand, "node_quarantined", &id, 60).await;
    assert_eq!(quarantined.len(), 1, "{quarantined:?}");
    let recovered = wait_event(stand, "node_recovered", &id, 90).await;
    assert_eq!(recovered.len(), 1, "{recovered:?}");
    stand
        .wait_state_in(&id, &["healthy"], secs(30))
        .await
        .expect("the node is healthy after the way back");
    wait_flag(stand, &id, "globalping_disputed", true, 30).await;
    (node, id)
}

/// Globalping alone says blocked, RedFlag and check-host see the node: it stays in the pool, marked
/// as disputed, and nothing bans it.
#[tokio::test]
#[ignore = "needs Docker; run with --ignored"]
async fn false_globalping_returns_after_one_clean_round() {
    let stand = Stand::up().await.expect("the stand comes up");
    let (node, id) = disputed_node(&stand).await;

    // `disputed_node` has seen quarantine, then recovery, then the flag. What decided it is the
    // round of the way back.
    let round = wait_round(&stand, &id, "the round that decided", 30, |r| {
        vote_in(r, "globalping").is_some_and(|v| v.0 == "blocked")
    })
    .await;
    assert_eq!(
        vote_in(&round, "red_flag").map(|v| v.0).as_deref(),
        Some("clear"),
        "{round}"
    );
    assert_eq!(
        vote_in(&round, "check_host").map(|v| v.0).as_deref(),
        Some("clear"),
        "{round}"
    );
    assert!(stand.bans().await.unwrap().is_empty());

    // The stand-in was really asked, about this node and the port of its engine.
    let ip = node.ip().await.unwrap();
    let starts = check_host_starts(&stand).await;
    assert!(
        starts
            .iter()
            .any(|s| s["host"] == json!(ip) && s["port"] == json!(443)),
        "check-host was not asked about {ip}:443: {starts:?}"
    );
    let info = stand.node_info(&id).await.unwrap();
    assert_eq!(info["globalping_disputed"], json!(true), "{info}");
}

/// The Russian check repeats on a disputed node; a `Blocked` in it sends the node back to
/// quarantine. A round of a healthy node bans nobody, even with two `Blocked` votes in it.
#[tokio::test]
#[ignore = "needs Docker; run with --ignored"]
async fn a_disputed_node_returns_to_quarantine_on_blocked() {
    let stand = Stand::up().await.expect("the stand comes up");
    let (_node, id) = disputed_node(&stand).await;

    // The repeated check comes with the next check of the agent, at most 20 s on a stand.
    settings(&stand, json!({"ru_recheck_sec": 20})).await;
    set_sensors(&stand, &[("check_host", "blocked")]).await;
    stand
        .wait_state_in(&id, &["quarantine"], secs(120))
        .await
        .expect("a Blocked from check-host in the repeated check quarantines the node");
    assert!(
        stand.bans().await.unwrap().is_empty(),
        "a round of a healthy node banned the node"
    );
    let round = wait_round(&stand, &id, "the round with the Blocked", 30, |r| {
        vote_in(r, "check_host").is_some_and(|v| v.0 == "blocked")
    })
    .await;
    assert_eq!(
        vote_in(&round, "red_flag").map(|v| v.0).as_deref(),
        Some("clear"),
        "{round}"
    );
    assert_eq!(
        stand.node_info(&id).await.unwrap()["suspected"],
        json!(true)
    );
}

/// Globalping and check-host both say blocked: that is two votes, and the node is banned.
#[tokio::test]
#[ignore = "needs Docker; run with --ignored"]
async fn globalping_and_check_host_ban() {
    let stand = Stand::up().await.expect("the stand comes up");
    // The check at enrolment sees a block; Globalping, still green, keeps the node healthy.
    set_sensors(&stand, &[("check_host", "blocked")]).await;
    let (_node, id) = enroll_healthy(&stand, 1, "node-one").await;
    assert!(stand.bans().await.unwrap().is_empty());

    set_sensors(&stand, &[("globalping", "blocked")]).await;
    wait_banned(&stand, &id, secs(120)).await;
    assert_eq!(
        stand.node_state(&id).await.unwrap(),
        None,
        "a banned node has no record"
    );
    let bans = stand.bans().await.unwrap();
    assert_eq!(bans.len(), 1, "{bans:?}");
    assert_eq!(bans[0]["node_id"], json!(id), "{bans:?}");
    let votes = bans[0]["votes"].as_array().expect("the votes of the ban");
    let blocked: Vec<_> = votes
        .iter()
        .filter(|v| v["vote"] == "blocked")
        .map(|v| v["sensor"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(
        blocked.contains(&"globalping".to_owned()) && blocked.contains(&"check_host".to_owned()),
        "the ban records both votes: {votes:?}"
    );
}

/// Another pool's filter on the port hides the node from Globalping's probes. The vote of Globalping
/// is then `Abstain(port_filter)`, and a ban needs both RedFlag and check-host.
#[tokio::test]
#[ignore = "needs Docker; run with --ignored"]
async fn a_filtered_node_needs_both_russian_checks_to_ban() {
    let stand = Stand::up().await.expect("the stand comes up");
    // A foreign rule on the engine's port with a set that holds only an address nobody uses:
    // it changes what the agent sees and blocks no check.
    stand
        .node(1)
        .exec(
            "ipset create e2e_foreign hash:ip && ipset add e2e_foreign 192.0.2.1 \
             && iptables -I INPUT -p tcp --dport 443 -m set --match-set e2e_foreign src -j DROP",
        )
        .await
        .expect("the foreign rule is put in place");
    set_sensors(&stand, &[("check_host", "ok")]).await;
    let (_node, id) = enroll_healthy(&stand, 1, "node-one").await;
    settings(&stand, json!({"ru_recheck_sec": 20})).await;

    // A healthy node is never banned by a round. RedFlag says blocked, check-host sees the node:
    // the node goes to quarantine, suspected. Globalping says blocked too and must not count.
    set_sensors(&stand, &[("globalping", "blocked"), ("redflag", "blocked")]).await;
    wait_event(&stand, "node_quarantined", &id, 60).await;
    assert!(stand.bans().await.unwrap().is_empty());
    let mut at = last_round_at(&stand, &id).await;

    // In quarantine: RedFlag `blocked` alone, check-host `clear`.
    let round = wait_round_after(&stand, &id, &at, "a round with RedFlag alone", 90, |r| {
        vote_in(r, "red_flag").is_some_and(|v| v.0 == "blocked")
            && vote_in(r, "check_host").is_some_and(|v| v.0 == "clear")
    })
    .await;
    assert_eq!(
        vote_in(&round, "globalping"),
        Some(("abstain".to_owned(), Some("port_filter".to_owned()))),
        "{round}"
    );
    assert_eq!(round["verdict"], json!("undecided"), "{round}");
    no_ban_and_still_in_quarantine(&stand, &id, "RedFlag alone").await;
    at = round["at"].as_str().unwrap().to_owned();

    // The two Russian checks must not say blocked in one round, so the way to the next case goes
    // through rounds in which neither does: check-host first, then RedFlag falls silent. A round
    // that sees check-host clear next to a silent RedFlag would let the node out of quarantine.
    set_sensors(&stand, &[("check_host", "refused")]).await;
    let round = wait_round_after(&stand, &id, &at, "check-host stops saying clear", 90, |r| {
        vote_in(r, "check_host").is_some_and(|v| v.0 == "abstain")
    })
    .await;
    set_sensors(&stand, &[("redflag", "unauthorized")]).await;
    let round = wait_round_after(
        &stand,
        &id,
        round["at"].as_str().unwrap(),
        "a round in which both abstain",
        90,
        |r| {
            vote_in(r, "red_flag").is_some_and(|v| v.0 == "abstain")
                && vote_in(r, "check_host").is_some_and(|v| v.0 == "abstain")
        },
    )
    .await;
    set_sensors(&stand, &[("check_host", "blocked")]).await;
    let round = wait_round_after(
        &stand,
        &id,
        round["at"].as_str().unwrap(),
        "check-host alone says blocked",
        90,
        |r| vote_in(r, "check_host").is_some_and(|v| v.0 == "blocked"),
    )
    .await;
    assert_eq!(
        vote_in(&round, "red_flag").map(|v| v.0).as_deref(),
        Some("abstain"),
        "{round}"
    );
    no_ban_and_still_in_quarantine(&stand, &id, "check-host alone").await;

    // Check-host `blocked` alone, RedFlag `clear`.
    set_sensors(&stand, &[("redflag", "ok")]).await;
    let round = wait_round_after(
        &stand,
        &id,
        round["at"].as_str().unwrap(),
        "check-host blocked next to a clear RedFlag",
        90,
        |r| {
            vote_in(r, "check_host").is_some_and(|v| v.0 == "blocked")
                && vote_in(r, "red_flag").is_some_and(|v| v.0 == "clear")
        },
    )
    .await;
    assert_eq!(round["verdict"], json!("undecided"), "{round}");
    no_ban_and_still_in_quarantine(&stand, &id, "check-host with a clear RedFlag").await;

    // Both: RedFlag says blocked now, check-host still does.
    set_sensors(&stand, &[("redflag", "blocked")]).await;
    wait_banned(&stand, &id, secs(120)).await;
    let bans = stand.bans().await.unwrap();
    assert_eq!(bans.len(), 1, "{bans:?}");
    let votes = bans[0]["votes"].as_array().expect("the votes of the ban");
    let blocked: Vec<_> = votes
        .iter()
        .filter(|v| v["vote"] == "blocked")
        .map(|v| v["sensor"].as_str().unwrap_or_default())
        .collect();
    assert!(
        blocked.contains(&"red_flag")
            && blocked.contains(&"check_host")
            && !blocked.contains(&"globalping"),
        "the ban rests on RedFlag and check-host: {votes:?}"
    );
}

async fn no_ban_and_still_in_quarantine(stand: &Stand, id: &str, what: &str) {
    assert!(stand.bans().await.unwrap().is_empty(), "{what} banned");
    assert_eq!(
        stand.node_state(id).await.unwrap().as_deref(),
        Some("quarantine"),
        "{what} moved the node out of quarantine"
    );
}

/// Every sensor says nothing useful: no ban, and the node is pruned when its quarantine runs out.
#[tokio::test]
#[ignore = "needs Docker; run with --ignored"]
async fn all_sensors_down_no_ban_then_pruned() {
    let stand = Stand::up().await.expect("the stand comes up");
    // The agent gets `limit_exceeded` from check-host and has nothing to report: no result, no vote.
    set_sensors(&stand, &[("check_host", "limited")]).await;
    let (_node, id) = enroll_healthy(&stand, 1, "node-one").await;
    settings(
        &stand,
        // The agent's report must not vote for Globalping in the rounds: they are about sensors
        // that are down. The hour of the spec is shortened to what a test can wait for.
        json!({
            "quarantine_max_sec": 300,
            "globalping": {"vote_report_max_age_sec": 1},
        }),
    )
    .await;

    // A failed check makes the node suspected; a stale report alone would not run sensors.
    set_sensors(
        &stand,
        &[("globalping", "blocked"), ("redflag", "unauthorized")],
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
        secs(240),
        || async {
            match last_round(&stand, &id).await {
                Some((verdict, votes))
                    if ["globalping", "check_host", "red_flag"]
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
    assert!(
        stand.bans().await.unwrap().is_empty(),
        "a ban without any vote"
    );
    stand
        .wait_state_in(&id, &["pruned"], secs(400))
        .await
        .expect("the node leaves quarantine and is pruned, not banned");
    assert!(stand.bans().await.unwrap().is_empty());
    assert!(
        stand.sensor_calls("check_host").await.unwrap() >= 1,
        "check-host was never asked"
    );
}

/// The port check-host is asked about is closed on the node: the registrar finds out itself and
/// the vote is an abstention, whatever check-host says.
#[tokio::test]
#[ignore = "needs Docker; run with --ignored"]
async fn a_closed_check_port_abstains() {
    let stand = Stand::up().await.expect("the stand comes up");
    set_sensors(&stand, &[("check_host", "blocked")]).await;
    let node = stand.node(1);
    let id = node
        .enroll_checking_port("node-one", 2222)
        .await
        .expect("the node enrolls");
    stand
        .wait_state(&id, "healthy", secs(120))
        .await
        .expect("the node becomes healthy");
    set_sensors(
        &stand,
        &[("globalping", "blocked"), ("redflag", "unauthorized")],
    )
    .await;

    let round = wait_round(&stand, &id, "a round with a check-host vote", 240, |r| {
        vote_in(r, "check_host").is_some()
    })
    .await;
    assert_eq!(
        vote_in(&round, "check_host"),
        Some(("abstain".to_owned(), Some("port_closed".to_owned()))),
        "{round}"
    );
    assert_eq!(
        vote_in(&round, "globalping").map(|v| v.0).as_deref(),
        Some("blocked"),
        "{round}"
    );
    assert!(stand.bans().await.unwrap().is_empty());
    let ip = node.ip().await.unwrap();
    let starts = check_host_starts(&stand).await;
    assert!(
        starts
            .iter()
            .any(|s| s["host"] == json!(ip) && s["port"] == json!(2222)),
        "check-host was not asked about the configured port: {starts:?}"
    );
}

/// What one node of `answers_without_a_vote` was enrolled under.
struct Probe {
    mode: &'static str,
    id: String,
    /// The id the stand-in gave to the check of this node (none when it refused to start one).
    check_id: Option<String>,
    ip: String,
}

/// Answers of check-host that are not a clear sign of a block or of a free address give no vote,
/// and a `Blocked` from Globalping next to them bans nobody. The nodes of one stand are each
/// enrolled under another mode of the stand-in, since the first check of a node is the one its
/// first round uses. The stand-in reads the mode when the registrar fetches an answer, so each
/// node is given time to have its answer read (or given up on) before the mode changes, and the
/// reads are counted to prove which mode was answered.
async fn answers_without_a_vote(modes: &[&'static str]) {
    let stand = Stand::up().await.expect("the stand comes up");
    let mut probes: Vec<Probe> = Vec::new();
    for (i, mode) in modes.iter().enumerate() {
        set_sensors(&stand, &[("check_host", mode)]).await;
        let n = u32::try_from(i).unwrap() + 1;
        let (node, id) = enroll_healthy(&stand, n, &format!("node-{n}")).await;
        let ip = node.ip().await.unwrap();
        let mut probe = Probe {
            mode,
            id,
            check_id: None,
            ip: ip.clone(),
        };
        if *mode == "limited" {
            eventually("the start is turned away", secs(30), || async {
                let limited = check_host_limited(&stand).await;
                if limited.iter().any(|l| l["host"] == json!(ip)) {
                    Ok(())
                } else {
                    Err(format!("{limited:?}"))
                }
            })
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        } else {
            let check_id = eventually("the check of this node is started", secs(30), || async {
                let starts = check_host_starts(&stand).await;
                starts
                    .iter()
                    .find(|s| s["host"] == json!(ip) && s["mode"] == json!(mode))
                    .and_then(|s| s["id"].as_str().map(str::to_owned))
                    .ok_or_else(|| format!("{starts:?}"))
            })
            .await
            .unwrap_or_else(|e| panic!("{e}"));
            let second_of_a_shared_id = *mode == "same_id" && i > 0;
            if second_of_a_shared_id {
                // The id is taken: the registrar does not read an answer for this start. Time to
                // see that it does not.
                tokio::time::sleep(secs(5)).await;
            } else {
                eventually("the answer is read", secs(30), || async {
                    let n = check_host_fetches(&stand, &check_id).await.len();
                    if n >= 1 {
                        Ok(())
                    } else {
                        Err("no read yet".to_owned())
                    }
                })
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            }
            if *mode == "garbage" {
                // An unparsable answer is read again for 30 s before the registrar gives up. The
                // mode must stay until it has.
                tokio::time::sleep(secs(34)).await;
            }
            probe.check_id = Some(check_id);
        }
        probes.push(probe);
    }
    set_sensors(
        &stand,
        &[("globalping", "blocked"), ("redflag", "unauthorized")],
    )
    .await;

    // The first round of each node is the one that uses the check made at its enrolment; the later
    // ones ask for new checks, in the mode the stand-in has by then. All nodes are watched at
    // once so that no first round is missed.
    let mut rounds: Vec<Option<serde_json::Value>> = vec![None; probes.len()];
    let seen = std::sync::Mutex::new(&mut rounds);
    eventually("a first round of every node", secs(200), || async {
        let mut all = true;
        for (index, probe) in probes.iter().enumerate() {
            if seen.lock().unwrap()[index].is_some() {
                continue;
            }
            let info = stand
                .node_info(&probe.id)
                .await
                .map_err(|e| e.to_string())?;
            let round = &info["last_round"];
            if !round.is_null()
                && vote_in(round, "globalping").is_some_and(|v| v.0 == "blocked")
                && vote_in(round, "check_host").is_some()
            {
                seen.lock().unwrap()[index] = Some(round.clone());
            } else {
                all = false;
            }
        }
        if all {
            Ok(())
        } else {
            Err("not every node had a round".to_owned())
        }
    })
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    drop(seen);

    for (index, probe) in probes.iter().enumerate() {
        let round = rounds[index].as_ref().expect("a round");
        let (expected, reason) = match probe.mode {
            "refused" | "mixed" => ("abstain", Some("unknown_answer")),
            "limited" => ("abstain", Some("rate_limited")),
            "garbage" | "foreign" => ("abstain", Some("no_data")),
            // The first node under `same_id` owns the shared id: its result counts, the second
            // one is refused.
            "same_id" if index == 0 => ("clear", None),
            "same_id" => ("abstain", Some("no_data")),
            other => panic!("no expectation for {other}"),
        };
        assert_eq!(
            vote_in(round, "check_host"),
            Some((expected.to_owned(), reason.map(str::to_owned))),
            "{}: {round}",
            probe.mode
        );
        let reads = match &probe.check_id {
            Some(check_id) => check_host_fetches(&stand, check_id).await,
            None => Vec::new(),
        };
        let all_in_mode = reads.iter().all(|r| r["mode"] == json!(probe.mode));
        match (probe.mode, index) {
            ("limited", _) => {
                let limited = check_host_limited(&stand).await;
                assert!(
                    limited.iter().any(|l| l["host"] == json!(probe.ip)),
                    "the start of {} was not turned away: {limited:?}",
                    probe.ip
                );
            }
            // Read again and again for the 30 s the registrar waits for an answer it can parse.
            ("garbage", _) => assert!(reads.len() >= 5 && all_in_mode, "{reads:?}"),
            // A foreign host is rejected at the first read.
            ("foreign", _) => assert!(reads.len() == 1 && all_in_mode, "{reads:?}"),
            // The id belongs to the first node: one read, and none for the start that lost it.
            ("same_id", 0) => assert!(reads.len() == 1 && all_in_mode, "{reads:?}"),
            ("same_id", _) => assert!(
                reads.len() == 1,
                "the second start under the shared id was read: {reads:?}"
            ),
            _ => assert!(
                !reads.is_empty() && all_in_mode,
                "{}: {reads:?}",
                probe.mode
            ),
        }
    }
    assert!(
        stand.bans().await.unwrap().is_empty(),
        "an answer without a vote banned a node"
    );
}

#[tokio::test]
#[ignore = "needs Docker; run with --ignored"]
async fn check_host_answers_that_give_no_vote() {
    tokio::join!(
        answers_without_a_vote(&["refused", "mixed", "limited"]),
        answers_without_a_vote(&["garbage", "foreign"]),
        answers_without_a_vote(&["same_id", "same_id"]),
    );
}

/// A Globalping answer of HTTP 403 over a finished TLS handshake is a success.
#[tokio::test]
#[ignore = "needs Docker; run with --ignored"]
async fn gp_403_with_tls_healthy() {
    let stand = Stand::up().await.expect("the stand comes up");
    // No result from check-host: only RedFlag could confirm the node from Russia, and it will not.
    set_sensors(&stand, &[("check_host", "limited")]).await;
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
    set_sensors(
        &stand,
        &[("globalping", "blocked"), ("redflag", "unauthorized")],
    )
    .await;
    stand
        .wait_state_in(&id, &["quarantine"], secs(60))
        .await
        .expect("a failed check quarantines the node");
    set_sensors(&stand, &[("globalping", "ok"), ("redflag", "ok")]).await;
    stand
        .wait_state_in(&id, &["healthy"], secs(120))
        .await
        .expect("the 403 with TLS brings the node back");
    // The round that brought the node back may still hold the vote of the failed check. What
    // shows that the 403 with TLS counts as a success is the agent's next report: the node is
    // no longer marked as disputed.
    wait_flag(&stand, &id, "globalping_disputed", false, 60).await;
    assert_eq!(
        stand.node_state(&id).await.unwrap().as_deref(),
        Some("healthy")
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

/// A banned address whose node asks again is checked once more: a dirty answer keeps the ban, a
/// clean one lifts it, and a node that comes back can be banned again.
#[tokio::test]
#[ignore = "needs Docker; run with --ignored"]
async fn reverify_clean_lifts_dirty_keeps() {
    let stand = Stand::up().await.expect("the stand comes up");
    // The check the agent makes at enrolment sees a block.
    set_sensors(&stand, &[("check_host", "blocked")]).await;
    let (_node, id) = enroll_healthy(&stand, 1, "node-one").await;
    settings(&stand, json!({"reverify_pause_sec": 10})).await;

    // Two sensors say blocked: the node is banned.
    set_sensors(&stand, &[("globalping", "blocked")]).await;
    wait_banned(&stand, &id, secs(120)).await;

    // The agent lost its token with the record and, the pause over, asks the pool for a place by
    // itself. The address is still blocked: the re-check keeps the ban.
    eventually("the reverify fails", secs(120), || async {
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
    // keeps the dirty verdict. The agent keeps asking after each pause, so there may be more.
    assert!(
        bans.iter().all(|b| b["lifted_at"].is_null()),
        "a dirty re-check lifts nothing: {bans:?}"
    );
    let recheck = bans
        .iter()
        .find(|b| b["reverify"] == true)
        .unwrap_or_else(|| panic!("a re-check row is open: {bans:?}"));
    let kept_votes = recheck["votes"]
        .as_array()
        .expect("the votes of the re-check");
    let blocked_by = |sensor: &str| {
        kept_votes
            .iter()
            .any(|x| x["sensor"] == sensor && x["vote"] == "blocked")
    };
    assert!(
        blocked_by("globalping") && blocked_by("check_host"),
        "the re-check keeps both blocked votes: {recheck:?}"
    );
    assert!(
        stand
            .events(Some("ban_lifted"), None)
            .await
            .unwrap()
            .is_empty(),
        "no ban was lifted yet"
    );

    // The address is clean again: the next re-check lifts the ban.
    set_sensors(
        &stand,
        &[
            ("globalping", "ok"),
            ("redflag", "ok"),
            ("check_host", "ok"),
        ],
    )
    .await;
    eventually("the ban is lifted", secs(120), || async {
        match stand.events(Some("ban_lifted"), None).await {
            Ok(events) if !events.is_empty() => Ok(()),
            Ok(_) => Err("no ban_lifted event yet".to_owned()),
            Err(e) => Err(e.to_string()),
        }
    })
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    let lifted = stand.events(Some("ban_lifted"), None).await.unwrap();
    assert_eq!(lifted.len(), 1, "{lifted:?}");
    let bans = stand.bans().await.unwrap();
    assert!(
        bans.iter().any(|b| !b["lifted_at"].is_null()),
        "the lift is in the ban log: {bans:?}"
    );

    // The node that came back is in the pool again. The re-check that let it back used the clean
    // answers of Globalping and check-host.
    let back = healthy_node_other_than(&stand, &id).await;
    let round = wait_round(&stand, &back, "the re-check round", 30, |r| {
        vote_in(r, "check_host").is_some()
    })
    .await;
    assert_eq!(
        vote_in(&round, "check_host").map(|v| v.0).as_deref(),
        Some("clear"),
        "{round}"
    );
    assert_eq!(
        vote_in(&round, "globalping").map(|v| v.0).as_deref(),
        Some("clear"),
        "{round}"
    );

    // And a block that returns bans it once more: Globalping and check-host agree again.
    set_sensors(&stand, &[("check_host", "blocked")]).await;
    set_sensors(&stand, &[("globalping", "blocked")]).await;
    wait_banned(&stand, &back, secs(180)).await;
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
