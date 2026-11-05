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
