//! The client numbers of the pool at one moment, for the history chart.

use std::collections::BTreeMap;

use crate::lifecycle::NodeRecord;
use crate::state::PoolState;
use crate::types::{NodeId, NodeState, Ts};

/// The domain key of the pool total in a stored sample row.
pub const TOTAL_DOMAIN: &str = "";

/// How many clients the pool had at `at`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientSample {
    pub at: Ts,
    /// Clients of the nodes in Pending, Healthy and Quarantine.
    pub total: u64,
    /// Clients of each domain that is not folded, as `apply` keeps them in `srmd`. A domain
    /// without a known number has no entry.
    pub domains: BTreeMap<String, u64>,
}

impl ClientSample {
    /// The rows to store: the total under `TOTAL_DOMAIN` first, then one per domain.
    pub fn rows(&self) -> Vec<(String, u64)> {
        let mut rows = vec![(TOTAL_DOMAIN.to_owned(), self.total)];
        rows.extend(self.domains.iter().map(|(d, n)| (d.clone(), *n)));
        rows
    }
}

/// Reads the numbers off `state` at `now`. A node whose reports carry no client count keeps
/// the last known value in `NodeStats::clients`, and so does a domain in `SrmdState`.
pub fn client_sample(state: &PoolState, now: Ts) -> ClientSample {
    let total = counted_nodes(state)
        .filter_map(|n| state.stats.get(&n.id)?.clients)
        .fold(0, u64::saturating_add);
    ClientSample {
        at: now,
        total,
        domains: domain_clients(state),
    }
}

/// The nodes whose clients are the pool's.
fn counted_nodes(state: &PoolState) -> impl Iterator<Item = &NodeRecord> {
    state.nodes.values().filter(|n| {
        matches!(
            n.state,
            NodeState::Pending | NodeState::Healthy | NodeState::Quarantine
        )
    })
}

fn domain_clients(state: &PoolState) -> BTreeMap<String, u64> {
    // A folded domain is a CNAME onto another one: its clients are counted there.
    state
        .settings
        .domains
        .iter()
        .filter(|d| !state.srmd.cnames.contains_key(d.as_str()))
        .filter_map(|d| Some((d.clone(), *state.srmd.domain_clients.get(d)?)))
        .collect()
}

/// The numbers the public pages show live.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveCounts {
    /// The same number as [`ClientSample::total`].
    pub total: u64,
    pub domains: BTreeMap<String, u64>,
    /// Clients of the pool's users on each counted node with a known number.
    pub nodes: BTreeMap<NodeId, u64>,
    /// The owner's own users on each counted node that has any.
    pub other_nodes: BTreeMap<NodeId, u64>,
}

/// Reads the numbers of the public pages off `state`.
pub fn live_counts(state: &PoolState) -> LiveCounts {
    let mut counts = LiveCounts {
        domains: domain_clients(state),
        ..LiveCounts::default()
    };
    for node in counted_nodes(state) {
        let Some(stats) = state.stats.get(&node.id) else {
            continue;
        };
        if let Some(clients) = stats.clients {
            counts.total = counts.total.saturating_add(clients);
            counts.nodes.insert(node.id.clone(), clients);
        }
        if let Some(other) = stats.other_clients.filter(|n| *n > 0) {
            counts.other_nodes.insert(node.id.clone(), other);
        }
    }
    counts
}

/// One node as one tick saw it, for the availability history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSample {
    pub node: NodeId,
    /// Healthy counts as up; Quarantine and Dead as down.
    pub up: bool,
    pub clients: Option<u64>,
}

/// The nodes that count for availability at this moment. Pending, Banned and Pruned nodes
/// are left out: they were never in service, or were taken out on purpose.
pub fn node_samples(state: &PoolState) -> Vec<NodeSample> {
    state
        .nodes
        .values()
        .filter_map(|n| {
            let up = match n.state {
                NodeState::Healthy => true,
                NodeState::Quarantine | NodeState::Dead => false,
                _ => return None,
            };
            Some(NodeSample {
                node: n.id.clone(),
                up,
                clients: state.stats.get(&n.id).and_then(|s| s.clients),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;
    use crate::apply::apply;
    use crate::settings::Settings;
    use crate::state::{Command, EnrollRequest, NodeStats, ReplyId, ReportRequest};
    use crate::types::{EngineKind, NodeId, OwnerId};
    use crate::votes::Vote;

    const NOW: Ts = Ts(1_800_000_000_000);

    #[test]
    fn live_numbers_agree_with_the_sample_and_keep_the_owners_apart() {
        let mut state = PoolState::new(Settings::default());
        add(&mut state, "a", NodeState::Healthy, None);
        add(&mut state, "b", NodeState::Quarantine, None);
        add(&mut state, "c", NodeState::Dead, None);
        let id = |name: &str| NodeId::parse(&format!("{name}-a1b2c")).unwrap();
        let stats = |clients, other| NodeStats {
            clients: Some(clients),
            other_clients: Some(other),
            ..NodeStats::default()
        };
        state.stats.insert(id("a"), stats(5, 30));
        state.stats.insert(id("b"), stats(2, 0));
        state.stats.insert(id("c"), stats(9, 9));

        let live = live_counts(&state);
        assert_eq!(live.total, 7);
        assert_eq!(live.total, client_sample(&state, NOW).total);
        assert_eq!(live.nodes.len(), 2);
        assert_eq!(live.nodes[&id("a")], 5);
        assert_eq!(live.nodes[&id("b")], 2);
        assert_eq!(live.other_nodes.len(), 1);
        assert_eq!(live.other_nodes[&id("a")], 30);
    }

    #[test]
    fn only_the_pools_clients_reach_the_samples() {
        let mut state = PoolState::new(Settings::default());
        add(&mut state, "a", NodeState::Healthy, None);
        let node = NodeId::parse("a-a1b2c").unwrap();
        let generation = state.nodes[&node].generation;
        let report = Command::Report(ReportRequest {
            reply: ReplyId(1),
            node,
            generation,
            metrics_ok: true,
            globalping: Vote::Clear,
            traffic: None,
            clients: Some(4),
            other_traffic: None,
            other_clients: Some(100),
        });
        apply(&mut state, report, NOW);
        assert_eq!(client_sample(&state, NOW).total, 4);
        assert_eq!(node_samples(&state)[0].clients, Some(4));
    }

    fn add(state: &mut PoolState, name: &str, node_state: NodeState, clients: Option<u64>) {
        let id = NodeId::parse(&format!("{name}-a1b2c")).unwrap();
        let mut node = NodeRecord::new(
            id.clone(),
            OwnerId(1),
            Ipv4Addr::new(95, 216, 0, 1),
            443,
            EngineKind::Telemt,
            Ts(0),
            &Settings::default(),
        );
        node.state = node_state;
        state.nodes.insert(id.clone(), node);
        state.stats.insert(
            id,
            NodeStats {
                clients,
                ..NodeStats::default()
            },
        );
    }

    #[test]
    fn total_counts_pending_healthy_quarantine_not_banned_or_dead() {
        let mut state = PoolState::default();
        add(&mut state, "pend", NodeState::Pending, Some(1));
        add(&mut state, "good", NodeState::Healthy, Some(10));
        add(&mut state, "quar", NodeState::Quarantine, Some(100));
        add(&mut state, "banned", NodeState::Banned, Some(1000));
        add(&mut state, "dead", NodeState::Dead, Some(10_000));
        add(&mut state, "pruned", NodeState::Pruned, Some(100_000));
        assert_eq!(client_sample(&state, NOW).total, 111);
    }

    #[test]
    fn node_without_a_metric_uses_the_last_known_value() {
        let mut state = PoolState::new(Settings::default());
        let node = NodeId::parse("seen-a1b2c").unwrap();
        let enroll = Command::Enroll(EnrollRequest {
            reply: ReplyId(1),
            node_id: node.clone(),
            owner: OwnerId(1),
            ip: Ipv4Addr::new(95, 216, 0, 1),
            port: 443,
            engine: EngineKind::Telemt,
            limit: None,
        });
        apply(&mut state, enroll, NOW);
        let generation = state.nodes[&node].generation;
        for (reply, clients) in [(2, Some(4)), (3, None)] {
            let report = Command::Report(ReportRequest {
                reply: ReplyId(reply),
                node: node.clone(),
                generation,
                metrics_ok: true,
                globalping: Vote::Clear,
                traffic: None,
                clients,
                other_traffic: None,
                other_clients: None,
            });
            apply(&mut state, report, NOW);
        }
        assert_eq!(client_sample(&state, NOW).total, 4);

        // A node that never reported a number adds nothing.
        add(&mut state, "silent", NodeState::Healthy, None);
        assert_eq!(client_sample(&state, NOW).total, 4);
    }

    #[test]
    fn one_row_per_domain_and_one_total() {
        let mut state = PoolState::default();
        state.settings.domains = vec!["a.example.com".into(), "b.example.com".into()];
        state.srmd.domain_clients.insert("a.example.com".into(), 4);
        state.srmd.domain_clients.insert("b.example.com".into(), 6);
        state
            .srmd
            .domain_clients
            .insert("gone.example.com".into(), 99);
        add(&mut state, "good", NodeState::Healthy, Some(10));
        let sample = client_sample(&state, NOW);
        assert_eq!(sample.at, NOW);
        assert_eq!(
            sample.rows(),
            [
                (String::new(), 10),
                ("a.example.com".to_owned(), 4),
                ("b.example.com".to_owned(), 6),
            ]
        );
    }

    #[test]
    fn node_samples_count_healthy_up_quarantine_and_dead_down_and_skip_the_rest() {
        let mut state = PoolState::default();
        add(&mut state, "pend", NodeState::Pending, Some(1));
        add(&mut state, "good", NodeState::Healthy, Some(10));
        add(&mut state, "quar", NodeState::Quarantine, None);
        add(&mut state, "banned", NodeState::Banned, Some(1000));
        add(&mut state, "dead", NodeState::Dead, Some(7));
        add(&mut state, "pruned", NodeState::Pruned, Some(100_000));
        let mut samples = node_samples(&state);
        samples.sort_by(|a, b| a.node.cmp(&b.node));
        let view: Vec<_> = samples
            .iter()
            .map(|s| (s.node.as_str().split('-').next().unwrap(), s.up, s.clients))
            .collect();
        assert_eq!(
            view,
            [
                ("dead", false, Some(7)),
                ("good", true, Some(10)),
                ("quar", false, None),
            ]
        );
    }

    #[test]
    fn a_folded_domain_has_no_row() {
        let mut state = PoolState::default();
        state.settings.domains = vec!["a.example.com".into(), "b.example.com".into()];
        state.srmd.domain_clients.insert("a.example.com".into(), 9);
        state.srmd.domain_clients.insert("b.example.com".into(), 5);
        state
            .srmd
            .cnames
            .insert("b.example.com".into(), "a.example.com".into());
        let sample = client_sample(&state, NOW);
        assert_eq!(sample.domains.keys().collect::<Vec<_>>(), ["a.example.com"]);
    }
}
