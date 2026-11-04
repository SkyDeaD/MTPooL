//! The rollout of an agent release over the fleet: one canary, then batches. Pure: the caller
//! passes the nodes, the masters and the time, and stores the returned state.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::lifecycle::NodeRecord;
use crate::settings::RolloutSettings;
use crate::types::{NodeId, NodeState, Ts};

/// Why a rollout waits or stops. `code` is stable and machine-readable, `text` is for the
/// admin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reason {
    pub code: String,
    pub text: String,
}

/// Where a node is in the rollout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum Stage {
    /// Not touched yet.
    Idle,
    /// Told to update on its next poll.
    Offered { at: Ts },
    /// The agent said it began the update.
    Started { at: Ts },
    /// Running the new version.
    Updated { at: Ts },
    /// Did not take the update; never retried by this rollout.
    Failed { at: Ts, code: String },
    /// Offered but never started in time.
    Missed { at: Ts },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    /// The version the node last reported.
    pub version: Option<String>,
    #[serde(flatten)]
    pub stage: Stage,
}

/// What a paused rollout goes back to on "continue".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resume {
    Canary,
    Observing,
    Batches,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum Phase {
    /// One node takes the update first. Without a candidate the rollout waits here.
    Canary,
    /// The canary runs the new version and is watched.
    Observing {
        since: Ts,
    },
    /// Batches go out; the next one is due at `next_at`.
    Batches {
        next_at: Ts,
    },
    Paused {
        resume: Resume,
        at: Ts,
    },
    Done {
        at: Ts,
    },
    Cancelled {
        at: Ts,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rollout {
    pub id: i64,
    pub version: String,
    pub created_at: Ts,
    pub timings: RolloutSettings,
    pub phase: Phase,
    /// The reason the rollout waits or is paused; `None` while it runs.
    pub note: Option<Reason>,
    pub canary: Option<NodeId>,
    pub members: BTreeMap<NodeId, Member>,
}

/// What the agent tells about its update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Started,
    Updated,
    /// The update was refused or failed before the switch; the old version still runs.
    Failed {
        code: String,
    },
    /// The new version did not come up and the old one was put back.
    RolledBack,
}

impl Rollout {
    pub fn new(id: i64, version: &str, now: Ts, timings: RolloutSettings) -> Rollout {
        Rollout {
            id,
            version: version.to_owned(),
            created_at: now,
            timings,
            phase: Phase::Canary,
            note: None,
            canary: None,
            members: BTreeMap::new(),
        }
    }

    /// Not paused and not over.
    pub fn is_active(&self) -> bool {
        matches!(
            self.phase,
            Phase::Canary | Phase::Observing { .. } | Phase::Batches { .. }
        )
    }

    /// Not over: a paused rollout may still be continued.
    pub fn is_open(&self) -> bool {
        !matches!(self.phase, Phase::Done { .. } | Phase::Cancelled { .. })
    }

    /// The node polled with the version it runs.
    pub fn observe_version(&mut self, node: &NodeId, version: &str, now: Ts) {
        let current = !older(version, &self.version);
        let member = self.members.entry(node.clone()).or_insert(Member {
            version: None,
            stage: Stage::Idle,
        });
        member.version = Some(version.to_owned());
        if current && matches!(member.stage, Stage::Offered { .. } | Stage::Started { .. }) {
            member.stage = Stage::Updated { at: now };
        }
    }

    /// Whether the node should be told to update now.
    pub fn offer_for(&self, node: &NodeId) -> bool {
        self.is_active()
            && self
                .members
                .get(node)
                .is_some_and(|m| matches!(m.stage, Stage::Offered { .. }))
    }

    pub fn on_status(&mut self, node: &NodeId, status: &Status, now: Ts) {
        let Some(member) = self.members.get_mut(node) else {
            return;
        };
        match status {
            Status::Started => {
                if matches!(member.stage, Stage::Offered { .. } | Stage::Started { .. }) {
                    member.stage = Stage::Started { at: now };
                }
            }
            Status::Updated => {
                member.stage = Stage::Updated { at: now };
                member.version = Some(self.version.clone());
            }
            Status::Failed { code } => {
                member.stage = Stage::Failed {
                    at: now,
                    code: code.clone(),
                };
                let text = match code.as_str() {
                    "bad_signature" => format!("{node} rejected the release signature"),
                    "downgrade" => format!("{node} refused an older version"),
                    "key_mismatch" => format!("{node} refused a release signed with another key"),
                    _ => format!("{node} could not take the update ({code})"),
                };
                let code = match code.as_str() {
                    "bad_signature" => "bad_signature",
                    _ => "update_failed",
                };
                self.pause(code, text, now);
            }
            Status::RolledBack => {
                member.stage = Stage::Failed {
                    at: now,
                    code: "rolled_back".to_owned(),
                };
                let text = format!("{node} did not come up on the new version and was rolled back");
                self.pause("rolled_back", text, now);
            }
        }
    }

    fn pause(&mut self, code: &str, text: String, now: Ts) {
        let resume = match self.phase {
            Phase::Canary => Resume::Canary,
            Phase::Observing { .. } => Resume::Observing,
            Phase::Batches { .. } => Resume::Batches,
            _ => return,
        };
        self.phase = Phase::Paused { resume, at: now };
        self.note = Some(Reason {
            code: code.to_owned(),
            text,
        });
    }

    /// Moves the rollout on: checks the nodes, picks the canary, ends the observation, sends
    /// out the due batch.
    pub fn advance(&mut self, nodes: &[NodeRecord], masters: &BTreeSet<NodeId>, now: Ts) {
        if !self.is_active() {
            return;
        }
        self.check_nodes(nodes, now);
        if !self.is_active() {
            return;
        }
        // A canary that never started, or that gave up, is replaced.
        if let Some(c) = &self.canary
            && self
                .members
                .get(c)
                .is_none_or(|m| matches!(m.stage, Stage::Missed { .. } | Stage::Failed { .. }))
        {
            self.canary = None;
        }
        if matches!(self.phase, Phase::Canary) {
            self.advance_canary(nodes, masters, now);
        }
        if let Phase::Observing { since } = self.phase
            && now.0 >= since.0 + i64::from(self.timings.observe_sec) * 1000
        {
            self.phase = Phase::Batches { next_at: now };
        }
        if let Phase::Batches { next_at } = self.phase
            && now >= next_at
        {
            self.advance_batches(nodes, masters, now);
        }
    }

    fn check_nodes(&mut self, nodes: &[NodeRecord], now: Ts) {
        let return_ms = i64::from(self.timings.return_sec) * 1000;
        let wait_ms = i64::from(self.timings.offer_wait_sec) * 1000;
        let mut tripped: Option<(&'static str, String)> = None;
        for (id, m) in &mut self.members {
            match m.stage {
                Stage::Started { at } if now.0 - at.0 >= return_ms => {
                    m.stage = Stage::Failed {
                        at: now,
                        code: "node_not_back".to_owned(),
                    };
                    tripped.get_or_insert((
                        "node_not_back",
                        format!(
                            "{id} did not come back on the new version within {} s",
                            self.timings.return_sec
                        ),
                    ));
                }
                Stage::Offered { at } if now.0 - at.0 >= wait_ms => {
                    m.stage = Stage::Missed { at: now };
                }
                Stage::Updated { .. } => {
                    let down = nodes
                        .iter()
                        .find(|n| n.id == *id)
                        .is_some_and(|n| !is_up(n));
                    if down {
                        tripped.get_or_insert((
                            "health_drop",
                            format!("{id} is not healthy after the update"),
                        ));
                    }
                }
                _ => {}
            }
        }
        if let Some((code, text)) = tripped {
            self.pause(code, text, now);
        }
    }

    fn advance_canary(&mut self, nodes: &[NodeRecord], masters: &BTreeSet<NodeId>, now: Ts) {
        if let Some(c) = self.canary.clone() {
            if let Some(Member {
                stage: Stage::Updated { at },
                ..
            }) = self.members.get(&c)
            {
                self.phase = Phase::Observing { since: *at };
                self.note = None;
            }
            return;
        }
        let candidate = nodes
            .iter()
            .filter(|n| is_up(n) && !masters.contains(&n.id) && self.is_idle_outdated(&n.id))
            .map(|n| n.id.clone())
            .min();
        if let Some(c) = candidate {
            if let Some(m) = self.members.get_mut(&c) {
                m.stage = Stage::Offered { at: now };
            }
            self.canary = Some(c);
            self.note = None;
            return;
        }
        if self.finished(nodes, now) {
            self.finish(now);
        } else {
            self.note = Some(Reason {
                code: "no_canary_candidate".to_owned(),
                text: "no healthy node that is not a master is on an older version; waiting"
                    .to_owned(),
            });
        }
    }

    fn advance_batches(&mut self, nodes: &[NodeRecord], masters: &BTreeSet<NodeId>, now: Ts) {
        let batch = next_batch(self, nodes, masters, now);
        for id in &batch {
            if let Some(m) = self.members.get_mut(id) {
                m.stage = Stage::Offered { at: now };
            }
        }
        if batch.is_empty() && self.finished(nodes, now) {
            self.finish(now);
        } else {
            self.phase = Phase::Batches {
                next_at: Ts(now.0 + i64::from(self.timings.batch_interval_sec) * 1000),
            };
        }
    }

    fn finish(&mut self, now: Ts) {
        self.phase = Phase::Done { at: now };
        self.note = None;
    }

    fn is_idle_outdated(&self, id: &NodeId) -> bool {
        self.members.get(id).is_some_and(|m| {
            matches!(m.stage, Stage::Idle)
                && m.version
                    .as_deref()
                    .is_some_and(|v| older(v, &self.version))
        })
    }

    /// Nothing is left to do: no node waits for its turn or is under way, and every node has
    /// had time to report its version.
    fn finished(&self, nodes: &[NodeRecord], now: Ts) -> bool {
        let in_flight = self
            .members
            .values()
            .any(|m| matches!(m.stage, Stage::Offered { .. } | Stage::Started { .. }));
        let waiting = self.members.keys().any(|id| self.is_idle_outdated(id));
        let unheard = now.0 < self.created_at.0 + i64::from(self.timings.offer_wait_sec) * 1000
            && nodes
                .iter()
                .any(|n| is_up(n) && !self.members.contains_key(&n.id));
        !in_flight && !waiting && !unheard
    }

    /// "Continue" on a paused rollout. `false` when it is not paused.
    pub fn resume(&mut self, nodes: &[NodeRecord], now: Ts) -> bool {
        let Phase::Paused { resume, .. } = self.phase else {
            return false;
        };
        // A node that dropped is given up on, or the rollout would stop again at once.
        for (id, m) in &mut self.members {
            if matches!(m.stage, Stage::Updated { .. })
                && nodes
                    .iter()
                    .find(|n| n.id == *id)
                    .is_some_and(|n| !is_up(n))
            {
                m.stage = Stage::Failed {
                    at: now,
                    code: "health_drop".to_owned(),
                };
            }
        }
        self.note = None;
        self.phase = match resume {
            Resume::Canary => Phase::Canary,
            Resume::Observing => Phase::Observing { since: now },
            Resume::Batches => Phase::Batches { next_at: now },
        };
        true
    }

    /// `false` when the rollout is over already.
    pub fn cancel(&mut self, now: Ts) -> bool {
        if !self.is_open() {
            return false;
        }
        self.phase = Phase::Cancelled { at: now };
        self.note = None;
        true
    }
}

/// Healthy and alive: the nodes an update goes to and is judged by.
fn is_up(n: &NodeRecord) -> bool {
    n.state == NodeState::Healthy && n.not_alive_since.is_none()
}

/// The nodes the next batch takes: healthy, alive, on an older version, not touched yet;
/// non-masters first. At most `batch_percent` of the healthy fleet, at least one.
pub fn next_batch(
    rollout: &Rollout,
    nodes: &[NodeRecord],
    masters: &BTreeSet<NodeId>,
    now: Ts,
) -> Vec<NodeId> {
    match rollout.phase {
        Phase::Batches { next_at } if now >= next_at => {}
        _ => return Vec::new(),
    }
    let fleet = nodes
        .iter()
        .filter(|n| n.state == NodeState::Healthy)
        .count();
    let size = (fleet * rollout.timings.batch_percent as usize)
        .div_ceil(100)
        .max(1);
    let mut eligible: Vec<&NodeRecord> = nodes
        .iter()
        .filter(|n| is_up(n) && rollout.is_idle_outdated(&n.id))
        .collect();
    eligible.sort_by_key(|n| (masters.contains(&n.id), &n.id));
    eligible
        .into_iter()
        .take(size)
        .map(|n| n.id.clone())
        .collect()
}

/// `a` is an older version than `b`. Versions are `X.Y.Z` with an optional `-pre` part; text
/// that is not one is never older.
fn older(a: &str, b: &str) -> bool {
    match (parse(a), parse(b)) {
        (Some(a), Some(b)) => a < b,
        _ => false,
    }
}

/// A release sorts after a pre-release of the same numbers.
fn parse(v: &str) -> Option<((u64, u64, u64), bool, &str)> {
    let (core, pre) = match v.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (v, None),
    };
    let mut parts = core.split('.');
    let nums = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    if parts.next().is_some() {
        return None;
    }
    Some((nums, pre.is_none(), pre.unwrap_or("")))
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;
    use crate::settings::Settings;
    use crate::types::{EngineKind, OwnerId};

    const T0: Ts = Ts(1_000_000);

    fn at(sec: i64) -> Ts {
        Ts(T0.0 + sec * 1000)
    }

    fn id(n: usize) -> NodeId {
        NodeId::parse(&format!("n{n:02}-aaaaa")).unwrap()
    }

    fn node(n: usize, state: NodeState) -> NodeRecord {
        let mut r = NodeRecord::new(
            id(n),
            OwnerId(1),
            Ipv4Addr::new(95, 216, 0, n as u8 + 1),
            443,
            EngineKind::Telemt,
            T0,
            &Settings::default(),
        );
        r.state = state;
        r
    }

    fn fleet(count: usize) -> Vec<NodeRecord> {
        (0..count).map(|n| node(n, NodeState::Healthy)).collect()
    }

    fn started(count: usize) -> Rollout {
        let mut r = Rollout::new(1, "0.2.0", T0, RolloutSettings::default());
        for n in 0..count {
            r.observe_version(&id(n), "0.1.0", T0);
        }
        r
    }

    fn masters(ids: &[usize]) -> BTreeSet<NodeId> {
        ids.iter().map(|n| id(*n)).collect()
    }

    fn stage(r: &Rollout, n: usize) -> &Stage {
        &r.members[&id(n)].stage
    }

    #[test]
    fn canary_is_healthy_non_master() {
        let mut nodes = fleet(4);
        nodes[2].state = NodeState::Quarantine;
        let mut r = started(4);
        // 0 is a master, 1 is the first healthy non-master, 2 is in quarantine.
        r.advance(&nodes, &masters(&[0]), at(1));
        assert_eq!(r.canary, Some(id(1)));
        assert!(r.offer_for(&id(1)));
        for n in [0, 2, 3] {
            assert!(!r.offer_for(&id(n)), "only the canary is offered");
        }
        assert_eq!(r.phase, Phase::Canary);
    }

    #[test]
    fn canary_skips_a_node_that_is_not_alive() {
        let mut nodes = fleet(3);
        nodes[0].not_alive_since = Some(T0);
        let mut r = started(3);
        r.advance(&nodes, &masters(&[]), at(1));
        assert_eq!(r.canary, Some(id(1)));
    }

    #[test]
    fn canary_observed_10min() {
        let nodes = fleet(5);
        let mut r = started(5);
        r.advance(&nodes, &masters(&[0]), at(1));
        let canary = r.canary.clone().unwrap();
        r.on_status(&canary, &Status::Started, at(10));
        r.on_status(&canary, &Status::Updated, at(20));
        r.advance(&nodes, &masters(&[0]), at(21));
        assert_eq!(r.phase, Phase::Observing { since: at(20) });
        r.advance(&nodes, &masters(&[0]), at(20 + 599));
        assert_eq!(r.phase, Phase::Observing { since: at(20) });
        assert!(next_batch(&r, &nodes, &masters(&[0]), at(20 + 599)).is_empty());
        r.advance(&nodes, &masters(&[0]), at(20 + 600));
        assert!(matches!(r.phase, Phase::Batches { .. }));
    }

    /// Runs a rollout over `count` nodes until the first batch is due.
    fn in_batches(count: usize) -> (Rollout, Vec<NodeRecord>) {
        let nodes = fleet(count);
        let mut r = started(count);
        r.advance(&nodes, &masters(&[0]), at(1));
        let canary = r.canary.clone().unwrap();
        r.on_status(&canary, &Status::Updated, at(20));
        r.advance(&nodes, &masters(&[0]), at(21));
        r.advance(&nodes, &masters(&[0]), at(20 + 600));
        (r, nodes)
    }

    #[test]
    fn batches_10pct_every_5min() {
        let (mut r, nodes) = in_batches(20);
        let m = masters(&[0]);
        // 20 healthy nodes, 10% is 2; the batch went out as the observation ended.
        let offered = |r: &Rollout| (0..20).filter(|n| r.offer_for(&id(*n))).count();
        assert_eq!(offered(&r), 2, "first batch is 10% of 20");
        let next = match r.phase {
            Phase::Batches { next_at } => next_at,
            ref p => panic!("not in batches: {p:?}"),
        };
        assert_eq!(next, at(620 + 300));
        // Nothing before the interval has passed.
        r.advance(&nodes, &m, at(620 + 299));
        assert_eq!(offered(&r), 2);
        let taken: Vec<usize> = (0..20).filter(|n| r.offer_for(&id(*n))).collect();
        for n in taken {
            r.on_status(&id(n), &Status::Updated, at(640));
        }
        r.advance(&nodes, &m, at(620 + 300));
        assert_eq!(offered(&r), 2, "second batch is another 2");
    }

    #[test]
    fn small_fleet_batch_is_at_least_one() {
        let (r, _) = in_batches(5);
        let offered = (0..5).filter(|n| r.offer_for(&id(*n))).count();
        assert_eq!(offered, 1);
    }

    #[test]
    fn not_back_in_120s_pauses() {
        let (mut r, nodes) = in_batches(20);
        let m = masters(&[0]);
        let n = (0..20).find(|n| r.offer_for(&id(*n))).unwrap();
        r.on_status(&id(n), &Status::Started, at(630));
        r.advance(&nodes, &m, at(630 + 119));
        assert!(matches!(r.phase, Phase::Batches { .. }), "119 s is in time");
        r.advance(&nodes, &m, at(630 + 121));
        assert!(matches!(
            r.phase,
            Phase::Paused {
                resume: Resume::Batches,
                ..
            }
        ));
        assert_eq!(r.note.as_ref().unwrap().code, "node_not_back");
        assert!(r.note.as_ref().unwrap().text.contains(id(n).as_str()));
        assert!(matches!(stage(&r, n), Stage::Failed { .. }));
        // A paused rollout offers nothing.
        assert!((0..20).all(|n| !r.offer_for(&id(n))));
    }

    #[test]
    fn health_drop_pauses() {
        let (mut r, mut nodes) = in_batches(20);
        let m = masters(&[0]);
        let canary = r.canary.clone().unwrap();
        assert!(matches!(stage_of(&r, &canary), Stage::Updated { .. }));
        let i = (0..20).find(|n| id(*n) == canary).unwrap();
        nodes[i].not_alive_since = Some(at(700));
        r.advance(&nodes, &m, at(701));
        assert!(matches!(r.phase, Phase::Paused { .. }));
        assert_eq!(r.note.as_ref().unwrap().code, "health_drop");
    }

    fn stage_of<'a>(r: &'a Rollout, n: &NodeId) -> &'a Stage {
        &r.members[n].stage
    }

    #[test]
    fn health_drop_in_observation_pauses_and_resume_observes_again() {
        let nodes0 = fleet(5);
        let mut nodes = nodes0.clone();
        let m = masters(&[0]);
        let mut r = started(5);
        r.advance(&nodes, &m, at(1));
        let canary = r.canary.clone().unwrap();
        r.on_status(&canary, &Status::Updated, at(20));
        r.advance(&nodes, &m, at(21));
        let i = (0..5).find(|n| id(*n) == canary).unwrap();
        nodes[i].state = NodeState::Quarantine;
        r.advance(&nodes, &m, at(100));
        assert!(matches!(
            r.phase,
            Phase::Paused {
                resume: Resume::Observing,
                ..
            }
        ));
        assert!(r.resume(&nodes, at(200)));
        assert_eq!(r.phase, Phase::Observing { since: at(200) });
        assert!(r.note.is_none());
        // It does not pause again over the node it already gave up on.
        r.advance(&nodes, &m, at(201));
        assert!(matches!(r.phase, Phase::Observing { .. }));
    }

    #[test]
    fn failed_canary_pauses_and_resume_picks_another() {
        let nodes = fleet(4);
        let m = masters(&[0]);
        let mut r = started(4);
        r.advance(&nodes, &m, at(1));
        let canary = r.canary.clone().unwrap();
        r.on_status(
            &canary,
            &Status::Failed {
                code: "bad_signature".into(),
            },
            at(30),
        );
        assert!(matches!(
            r.phase,
            Phase::Paused {
                resume: Resume::Canary,
                ..
            }
        ));
        assert_eq!(r.note.as_ref().unwrap().code, "bad_signature");
        assert!(r.resume(&nodes, at(60)));
        r.advance(&nodes, &m, at(61));
        let again = r.canary.clone().unwrap();
        assert_ne!(again, canary, "the failed node is not tried again");
    }

    #[test]
    fn rolled_back_pauses() {
        let nodes = fleet(4);
        let m = masters(&[0]);
        let mut r = started(4);
        r.advance(&nodes, &m, at(1));
        let canary = r.canary.clone().unwrap();
        r.on_status(&canary, &Status::Started, at(5));
        r.on_status(&canary, &Status::RolledBack, at(50));
        assert_eq!(r.note.as_ref().unwrap().code, "rolled_back");
        assert!(matches!(r.phase, Phase::Paused { .. }));
    }

    #[test]
    fn resume_and_cancel() {
        let (mut r, nodes) = in_batches(20);
        let m = masters(&[0]);
        // Not paused: nothing to continue.
        assert!(!r.resume(&nodes, at(700)));
        let n = (0..20).find(|n| r.offer_for(&id(*n))).unwrap();
        r.on_status(
            &id(n),
            &Status::Failed {
                code: "download".into(),
            },
            at(640),
        );
        assert!(matches!(r.phase, Phase::Paused { .. }));
        assert!(r.resume(&nodes, at(800)));
        assert_eq!(r.phase, Phase::Batches { next_at: at(800) });
        assert!(r.note.is_none());
        r.advance(&nodes, &m, at(800));
        assert!((0..20).any(|n| r.offer_for(&id(n))));
        assert!(r.cancel(at(900)));
        assert_eq!(r.phase, Phase::Cancelled { at: at(900) });
        assert!(
            (0..20).all(|n| !r.offer_for(&id(n))),
            "a cancelled rollout offers nothing"
        );
        assert!(!r.cancel(at(901)), "already over");
        assert!(!r.resume(&nodes, at(902)));
        r.advance(&nodes, &m, at(5_000));
        assert_eq!(r.phase, Phase::Cancelled { at: at(900) });
    }

    #[test]
    fn no_canary_candidate_waits_and_explains() {
        // The only healthy node is the master.
        let mut nodes = fleet(3);
        nodes[1].state = NodeState::Quarantine;
        nodes[2].state = NodeState::Dead;
        let m = masters(&[0]);
        let mut r = started(3);
        r.advance(&nodes, &m, at(1));
        assert_eq!(r.canary, None);
        assert_eq!(r.phase, Phase::Canary);
        assert_eq!(r.note.as_ref().unwrap().code, "no_canary_candidate");
        assert!(!r.note.as_ref().unwrap().text.is_empty());
        // A node turns healthy: the rollout goes on by itself.
        nodes[1].state = NodeState::Healthy;
        r.advance(&nodes, &m, at(10));
        assert_eq!(r.canary, Some(id(1)));
        assert!(r.note.is_none());
    }

    #[test]
    fn offer_that_is_not_taken_is_missed_and_a_new_canary_is_picked() {
        let nodes = fleet(4);
        let m = masters(&[0]);
        let mut r = started(4);
        r.advance(&nodes, &m, at(1));
        let canary = r.canary.clone().unwrap();
        r.advance(&nodes, &m, at(1 + 301));
        assert!(matches!(stage_of(&r, &canary), Stage::Missed { .. }));
        assert_ne!(r.canary, Some(canary));
        assert!(r.canary.is_some());
    }

    #[test]
    fn nodes_on_the_new_version_are_left_alone_and_the_rollout_ends() {
        let nodes = fleet(3);
        let m = masters(&[0]);
        let mut r = Rollout::new(1, "0.2.0", T0, RolloutSettings::default());
        for n in 0..3 {
            r.observe_version(&id(n), "0.2.0", T0);
        }
        r.advance(&nodes, &m, at(1));
        assert_eq!(r.phase, Phase::Done { at: at(1) });
    }

    #[test]
    fn a_node_on_a_newer_version_is_not_offered_the_older_one() {
        let nodes = fleet(2);
        let m = masters(&[]);
        let mut r = Rollout::new(1, "0.2.0", T0, RolloutSettings::default());
        r.observe_version(&id(0), "0.3.0", T0);
        r.observe_version(&id(1), "0.1.0", T0);
        r.advance(&nodes, &m, at(1));
        assert_eq!(r.canary, Some(id(1)));
    }

    #[test]
    fn rollout_survives_a_json_round_trip() {
        let (r, _) = in_batches(8);
        let text = serde_json::to_string(&r).unwrap();
        let back: Rollout = serde_json::from_str(&text).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn the_last_batch_ends_the_rollout() {
        let (mut r, nodes) = in_batches(3);
        let m = masters(&[0]);
        let mut t = 620;
        for _ in 0..10 {
            for n in 0..3 {
                if r.offer_for(&id(n)) {
                    r.on_status(&id(n), &Status::Updated, at(t));
                }
            }
            t += 301;
            r.advance(&nodes, &m, at(t));
        }
        assert!(matches!(r.phase, Phase::Done { .. }), "{:?}", r.phase);
    }
}
