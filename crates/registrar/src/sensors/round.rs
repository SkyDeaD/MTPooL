//! The round runner: the driver that answers `Effect::RunRound`.
//!
//! A round asks every sensor about one node, all at once, and gives the votes back to the state
//! owner as `Command::RoundDone`. The state owner never waits for it: the driver runs on its own
//! task, and a node whose sensors are slow cannot hold up anything but its own round.
//!
//! * The whole round has one time limit; a sensor that has not answered by then, or that
//!   panics, gives an abstention and the others are kept.
//! * When the effect carries a fresh vote of the agent's own measurement, the central Globalping
//!   sensor is not asked: the fresh vote stands in its place.
//! * One round per node at a time. A second one for the same node is dropped; the state owner
//!   asks again on its next pass.

use std::collections::HashSet;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mtpool_core::{
    AbstainReason, Command, Effect, Generation, NodeId, SensorKind, SensorVote, Settings, Vote,
    link_secret,
};
use serde_json::json;

use super::{Sensor, Target};
use crate::actor::{ActorHandle, DriverFuture, EffectDriver};

#[cfg(test)]
mod tests;

/// The time a whole round may take for one node.
pub const SENSOR_TIMEOUT: Duration = Duration::from_secs(120);

/// What the sensors look at: the node's address and the pool's public face. The SNI is the TLS
/// domain; the secret is the first proxy user's, in link format. With no user there is no
/// secret and the sensors that need one abstain.
pub fn target_for(settings: &Settings, ip: Ipv4Addr, port: u16) -> Target {
    let sni = settings.tls_domain.clone();
    let secret = settings
        .proxy_users
        .first()
        .map(|user| link_secret(&user.secret_hex, &sni))
        .unwrap_or_default();
    Target {
        ip,
        port,
        sni,
        secret,
    }
}

fn abstain(reason: AbstainReason, why: &str) -> SensorVote {
    SensorVote {
        vote: Vote::Abstain(reason),
        evidence: json!({"reason": why}),
    }
}

/// Takes a place in the set of nodes with a round on the way and gives it back when dropped, even
/// when the round's task is cancelled half way.
struct Claim {
    set: Arc<Mutex<HashSet<NodeId>>>,
    node: NodeId,
}

impl Claim {
    fn take(set: &Arc<Mutex<HashSet<NodeId>>>, node: &NodeId) -> Option<Claim> {
        let mut guard = set.lock().unwrap_or_else(|e| e.into_inner());
        guard.insert(node.clone()).then(|| Claim {
            set: Arc::clone(set),
            node: node.clone(),
        })
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        let mut guard = self.set.lock().unwrap_or_else(|e| e.into_inner());
        guard.remove(&self.node);
    }
}

#[derive(Clone)]
pub struct RoundRunner {
    sensors: Vec<Arc<dyn Sensor>>,
    sensor_timeout: Duration,
    in_flight: Arc<Mutex<HashSet<NodeId>>>,
}

impl RoundRunner {
    pub fn new(sensors: Vec<Arc<dyn Sensor>>) -> RoundRunner {
        RoundRunner {
            sensors,
            sensor_timeout: SENSOR_TIMEOUT,
            in_flight: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// The time the whole round may take (`SENSOR_TIMEOUT` by default).
    pub fn with_sensor_timeout(mut self, timeout: Duration) -> RoundRunner {
        self.sensor_timeout = timeout;
        self
    }

    /// Asks every sensor at once and gathers the votes, in the order of the sensors. With a
    /// `fresh_gp` the Globalping sensor is left out and the fresh vote stands in its place (in
    /// front, when there is no such sensor).
    pub async fn round(
        &self,
        node: NodeId,
        generation: Generation,
        target: Target,
        fresh_gp: Option<SensorVote>,
    ) -> Command {
        // One deadline for the whole round: the sensors run together, so the round ends one
        // timeout after it began, however many of them are slow.
        let deadline = tokio::time::Instant::now() + self.sensor_timeout;
        let mut fresh = fresh_gp;
        let mut slots = Vec::new();
        for sensor in &self.sensors {
            let kind = sensor.kind();
            if kind == SensorKind::Globalping
                && let Some(vote) = fresh.take()
            {
                slots.push((kind, Slot::Ready(vote)));
                continue;
            }
            let sensor = Arc::clone(sensor);
            let target = target.clone();
            let task = tokio::spawn(async move { sensor.vote(&target).await });
            slots.push((kind, Slot::Running(task)));
        }
        let mut votes = Vec::with_capacity(slots.len() + 1);
        if let Some(vote) = fresh {
            votes.push((SensorKind::Globalping, vote));
        }
        for (kind, slot) in slots {
            let vote = match slot {
                Slot::Ready(vote) => vote,
                Slot::Running(mut task) => {
                    match tokio::time::timeout_at(deadline, &mut task).await {
                        Ok(Ok(vote)) => vote,
                        Ok(Err(error)) => {
                            tracing::warn!(?kind, %error, "a sensor failed during a round");
                            abstain(AbstainReason::Unavailable, "sensor_failed")
                        }
                        Err(_) => {
                            task.abort();
                            abstain(AbstainReason::Timeout, "timeout")
                        }
                    }
                }
            };
            votes.push((kind, vote));
        }
        Command::RoundDone {
            node,
            generation,
            votes,
        }
    }
}

enum Slot {
    Ready(SensorVote),
    Running(tokio::task::JoinHandle<SensorVote>),
}

impl EffectDriver for RoundRunner {
    fn handles(&self, effect: &Effect) -> bool {
        matches!(effect, Effect::RunRound { .. })
    }

    fn run(&self, effect: Effect, results: ActorHandle) -> DriverFuture {
        let Effect::RunRound {
            node,
            generation,
            ip,
            port,
            fresh_gp,
        } = effect
        else {
            return Box::pin(async {});
        };
        let this = self.clone();
        Box::pin(async move {
            let Some(_claim) = Claim::take(&this.in_flight, &node) else {
                tracing::warn!(%node, "a round for the node is already running, dropped");
                return;
            };
            // The settings are read when the round starts, so a changed domain or secret is used
            // by the next round without a restart.
            let Ok(target) = results
                .inspect(move |s| target_for(&s.settings, ip, port))
                .await
            else {
                return;
            };
            let command = this.round(node, generation, target, fresh_gp).await;
            if results.send(command).await.is_err() {
                tracing::debug!("the state owner is gone, the round result is dropped");
            }
        })
    }
}
