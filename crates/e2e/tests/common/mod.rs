//! Helpers the scenario files share.
#![allow(dead_code)]

use std::time::Duration;

use mtpool_e2e::{Node, Stand, eventually};
use serde_json::Value;

pub fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// Starts the agent of node `n`, and waits until the registrar calls the node healthy.
pub async fn enroll_healthy<'a>(stand: &'a Stand, n: u32, name: &str) -> (Node<'a>, String) {
    let node = stand.node(n);
    let id = node
        .enroll(name)
        .await
        .unwrap_or_else(|e| panic!("{name} enrolls: {e}"));
    stand
        .wait_state(&id, "healthy", secs(120))
        .await
        .unwrap_or_else(|e| panic!("{name} becomes healthy: {e}"));
    (node, id)
}

pub async fn set_sensors(stand: &Stand, modes: &[(&str, &str)]) {
    for (sensor, mode) in modes {
        stand
            .set_sensor(sensor, mode)
            .await
            .unwrap_or_else(|e| panic!("set {sensor} to {mode}: {e}"));
    }
}

pub async fn settings(stand: &Stand, patch: Value) {
    stand
        .admin
        .patch("/api/v1/admin/settings", patch)
        .await
        .unwrap_or_else(|e| panic!("settings change refused: {e}"));
}

/// The vote `sensor` gave in the last round of the node, if there was a round and the sensor
/// voted in it: `blocked`, `clear` or `abstain`.
pub async fn last_vote(stand: &Stand, node_id: &str, sensor: &str) -> Option<String> {
    let info = stand.node_info(node_id).await.ok()?;
    info["last_round"]["votes"]
        .as_array()?
        .iter()
        .find(|v| v["sensor"] == sensor)
        .and_then(|v| v["vote"].as_str().map(str::to_owned))
}

/// Waits until the sensor stand-in has counted `at_least` more calls than `before`.
pub async fn wait_calls(stand: &Stand, sensor: &str, before: u64, at_least: u64, within: u64) {
    eventually(
        &format!("{sensor} asked {at_least} more times"),
        secs(within),
        || async {
            match stand.sensor_calls(sensor).await {
                Ok(n) if n >= before + at_least => Ok(()),
                Ok(n) => Err(format!("{n} calls, {before} before")),
                Err(e) => Err(e.to_string()),
            }
        },
    )
    .await
    .unwrap_or_else(|e| panic!("{e}"));
}

/// The verdict of the last round of the node and what each sensor voted in it.
pub async fn last_round(stand: &Stand, node_id: &str) -> Option<(String, Vec<(String, String)>)> {
    let info = stand.node_info(node_id).await.ok()?;
    let round = info.get("last_round").filter(|r| !r.is_null())?;
    let votes = round["votes"]
        .as_array()?
        .iter()
        .filter_map(|v| {
            Some((
                v["sensor"].as_str()?.to_owned(),
                v["vote"].as_str()?.to_owned(),
            ))
        })
        .collect();
    Some((round["verdict"].as_str()?.to_owned(), votes))
}

pub fn vote_of(votes: &[(String, String)], sensor: &str) -> Option<String> {
    votes
        .iter()
        .find(|(s, _)| s == sensor)
        .map(|(_, v)| v.clone())
}

/// Runs the registrar build of `version`, as a rollback or an update leaves it: the binary replaced,
/// the service restarted, the database and the releases directory as they were.
pub async fn run_registrar_build(stand: &Stand, version: &str) {
    let run = stand
        .registrar_exec(&format!(
            "set -e; cd /usr/local/bin; cp /opt/e2e/registrar-{version} mtpool-registrar.new; \
             mv -f mtpool-registrar.new mtpool-registrar"
        ))
        .await
        .expect("the build is put in place");
    assert!(run.success, "{}", run.output);
    stand
        .restart_registrar()
        .await
        .expect("the registrar is back");
    let run = stand
        .registrar_exec("mtpool-registrar version")
        .await
        .expect("the registrar answers");
    assert!(run.output.starts_with(version), "{}", run.output);
}

/// What `sensor` voted in a round of the public node page: the vote and, for an abstention, why.
pub fn vote_in(round: &Value, sensor: &str) -> Option<(String, Option<String>)> {
    let vote = round["votes"]
        .as_array()?
        .iter()
        .find(|v| v["sensor"] == sensor)?;
    Some((
        vote["vote"].as_str()?.to_owned(),
        vote["reason"].as_str().map(str::to_owned),
    ))
}

/// Waits until the last round of the node satisfies `accept`, and returns it.
pub async fn wait_round(
    stand: &Stand,
    node_id: &str,
    what: &str,
    within: u64,
    accept: impl Fn(&Value) -> bool,
) -> Value {
    eventually(what, secs(within), || async {
        match stand.node_info(node_id).await {
            Ok(info) if !info["last_round"].is_null() && accept(&info["last_round"]) => {
                Ok(info["last_round"].clone())
            }
            Ok(info) => Err(info["last_round"].to_string()),
            Err(e) => Err(e.to_string()),
        }
    })
    .await
    .unwrap_or_else(|e| panic!("{e}"))
}

/// Waits until the public page of the node says `field` is `value`.
pub async fn wait_flag(stand: &Stand, node_id: &str, field: &str, value: bool, within: u64) {
    eventually(&format!("{field} is {value}"), secs(within), || async {
        match stand.node_info(node_id).await {
            Ok(info) if info[field] == value => Ok(()),
            Ok(info) => Err(format!("{}: {}", info["state"], info[field])),
            Err(e) => Err(e.to_string()),
        }
    })
    .await
    .unwrap_or_else(|e| panic!("{e}"));
}

/// The check-host checks the stand-in was asked to start: `{id, host, port, mode}` in order.
pub async fn check_host_starts(stand: &Stand) -> Vec<Value> {
    let state = stand.fakes_state().await.expect("the stand-ins answer");
    state["check_host"]["starts"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// The reads of an answer the stand-in served for the check `check_id`: `{id, mode}` in order,
/// the mode being the one it answered in.
pub async fn check_host_fetches(stand: &Stand, check_id: &str) -> Vec<Value> {
    let state = stand.fakes_state().await.expect("the stand-ins answer");
    state["check_host"]["fetches"]
        .as_array()
        .map(|all| {
            all.iter()
                .filter(|f| f["id"] == check_id)
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// The starts the stand-in turned away with `limit_exceeded`: `{host, port}` in order.
pub async fn check_host_limited(stand: &Stand) -> Vec<Value> {
    let state = stand.fakes_state().await.expect("the stand-ins answer");
    state["check_host"]["limited"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// Waits for an event of `kind` about the node and returns the events found, newest first.
pub async fn wait_event(stand: &Stand, kind: &str, node_id: &str, within: u64) -> Vec<Value> {
    eventually(&format!("an event {kind}"), secs(within), || async {
        match stand.events(Some(kind), Some(node_id)).await {
            Ok(events) if !events.is_empty() => Ok(events),
            Ok(_) => Err(format!("no {kind} event yet")),
            Err(e) => Err(e.to_string()),
        }
    })
    .await
    .unwrap_or_else(|e| panic!("{e}"))
}

/// Waits for a round of the node that is newer than `after` (the `at` of an earlier round, or an
/// empty string) and that `accept` takes, and returns it.
pub async fn wait_round_after(
    stand: &Stand,
    node_id: &str,
    after: &str,
    what: &str,
    within: u64,
    accept: impl Fn(&Value) -> bool,
) -> Value {
    wait_round(stand, node_id, what, within, |r| {
        r["at"].as_str().is_some_and(|at| at > after) && accept(r)
    })
    .await
}

/// The time of the latest round of the node, or an empty string when it had none.
pub async fn last_round_at(stand: &Stand, node_id: &str) -> String {
    stand
        .node_info(node_id)
        .await
        .ok()
        .and_then(|info| info["last_round"]["at"].as_str().map(str::to_owned))
        .unwrap_or_default()
}
