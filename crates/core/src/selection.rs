//! Master selection: which healthy nodes back each proxy domain.
//!
//! Pure functions over the pool state. `select` is meant to run on every tick, and the same
//! input always gives the same output. It follows section 6 of the spec:
//!
//! 1. Surplus slots (the limit was lowered) are dropped.
//! 2. A slot whose node is no longer `Healthy` is handed to a replacement.
//! 3. Free slots are filled.
//! 4. A slot held longer than the rotation TTL is swapped, one slot per domain per tick.
//! 5. A node without domains takes one slot from the most loaded node.
//!
//! A candidate is the least loaded `Healthy` node, then the first in the queue. A node never
//! holds a domain twice and two slots of one domain never share an IP. A swap or a refill
//! happens only when such a candidate exists: without one the slot stays where it is, so an
//! empty queue never clears the assignments.

use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;

use crate::lifecycle::NodeRecord;
use crate::settings::Settings;
use crate::types::{NodeId, NodeState, Ts};

/// One master of a domain and since when it holds the slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    pub node: NodeId,
    pub since: Ts,
}

/// Domain to its master slots. The position of a slot in the list is stable: a replacement
/// takes the place of the node it replaces.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Assignments(pub BTreeMap<String, Vec<Slot>>);

/// A slot that changed hands. Fill has no `removed`, a dropped surplus slot has no `added`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MasterChange {
    pub domain: String,
    pub added: Option<NodeId>,
    pub removed: Option<NodeId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionResult {
    pub assignments: Assignments,
    pub changes: Vec<MasterChange>,
    /// Nodes that lost a slot to the rotation TTL. `select` does not touch the nodes: the
    /// caller sends them to the back of the queue by setting `queued_at`.
    pub rotated_out: Vec<NodeId>,
}

/// Healthy nodes in queue order: `queued_at`, then `registered_at`, then the node id.
pub fn queue_order(nodes: &[NodeRecord]) -> Vec<NodeId> {
    let mut queue: Vec<&NodeRecord> = nodes
        .iter()
        .filter(|n| n.state == NodeState::Healthy)
        .collect();
    queue.sort_by_key(|n| (n.queued_at, n.registered_at, &n.id));
    queue.into_iter().map(|n| n.id.clone()).collect()
}

pub fn select(
    prev: &Assignments,
    nodes: &[NodeRecord],
    domains: &[String],
    now: Ts,
    s: &Settings,
) -> SelectionResult {
    let slots = domains
        .iter()
        .map(|d| (d.clone(), prev.0.get(d).cloned().unwrap_or_default()))
        .collect();
    let mut round = Round::new(slots, nodes, now, s.masters_per_domain as usize);
    round.trim_surplus();
    round.replace_unhealthy();
    round.fill_free();
    round.rotate(s.rotation_ttl_sec);
    round.rebalance_idle();
    round.finish()
}

/// The working state of one tick.
struct Round<'a> {
    now: Ts,
    masters: usize,
    nodes: BTreeMap<&'a NodeId, &'a NodeRecord>,
    /// Healthy nodes in queue order.
    queue: Vec<&'a NodeRecord>,
    /// Nodes rotated out during this tick: as if their `queued_at` were `now`.
    requeued: BTreeMap<NodeId, Ts>,
    slots: BTreeMap<String, Vec<Slot>>,
    /// Number of domains each node masters.
    load: BTreeMap<NodeId, usize>,
    changes: Vec<MasterChange>,
    rotated_out: Vec<NodeId>,
}

impl<'a> Round<'a> {
    fn new(
        slots: BTreeMap<String, Vec<Slot>>,
        nodes: &'a [NodeRecord],
        now: Ts,
        masters: usize,
    ) -> Round<'a> {
        let mut load = BTreeMap::new();
        for slot in slots.values().flatten() {
            *load.entry(slot.node.clone()).or_insert(0) += 1;
        }
        let mut round = Round {
            now,
            masters,
            nodes: nodes.iter().map(|n| (&n.id, n)).collect(),
            queue: nodes
                .iter()
                .filter(|n| n.state == NodeState::Healthy)
                .collect(),
            requeued: BTreeMap::new(),
            slots,
            load,
            changes: Vec::new(),
            rotated_out: Vec::new(),
        };
        round.sort_queue();
        round
    }

    fn finish(self) -> SelectionResult {
        SelectionResult {
            assignments: Assignments(self.slots),
            changes: self.changes,
            rotated_out: self.rotated_out,
        }
    }

    fn sort_queue(&mut self) {
        let requeued = &self.requeued;
        self.queue.sort_by_key(|n| {
            let queued_at = requeued.get(&n.id).copied().unwrap_or(n.queued_at);
            (queued_at, n.registered_at, &n.id)
        });
    }

    fn domains(&self) -> Vec<String> {
        self.slots.keys().cloned().collect()
    }

    fn is_healthy(&self, id: &NodeId) -> bool {
        self.nodes
            .get(id)
            .is_some_and(|n| n.state == NodeState::Healthy)
    }

    fn load_of(&self, id: &NodeId) -> usize {
        self.load.get(id).copied().unwrap_or(0)
    }

    /// IPs held by the other slots of the domain: the slot at `skip` is being replaced.
    fn ips_except(&self, domain: &str, skip: Option<usize>) -> BTreeSet<Ipv4Addr> {
        self.slots[domain]
            .iter()
            .enumerate()
            .filter(|(i, _)| Some(*i) != skip)
            .filter_map(|(_, slot)| self.nodes.get(&slot.node).map(|n| n.ip))
            .collect()
    }

    /// The node for a slot of `domain`: the least loaded, then the first in the queue. It must
    /// not hold the domain already (the node being replaced included) and its IP must differ
    /// from the other slots of the domain.
    fn pick(&self, domain: &str, replacing: Option<usize>) -> Option<NodeId> {
        let holders = &self.slots[domain];
        let taken = self.ips_except(domain, replacing);
        self.queue
            .iter()
            .filter(|n| !holders.iter().any(|h| h.node == n.id) && !taken.contains(&n.ip))
            .min_by_key(|n| self.load_of(&n.id))
            .map(|n| n.id.clone())
    }

    fn release(&mut self, id: &NodeId) {
        if let Some(n) = self.load.get_mut(id) {
            *n = n.saturating_sub(1);
        }
    }

    fn acquire(&mut self, id: &NodeId) {
        *self.load.entry(id.clone()).or_insert(0) += 1;
    }

    /// Gives the slot at `idx` to `new` and returns the node that held it.
    fn swap(&mut self, domain: &str, idx: usize, new: NodeId) -> NodeId {
        let slot = Slot {
            node: new.clone(),
            since: self.now,
        };
        let old = std::mem::replace(
            &mut self.slots.get_mut(domain).expect("known domain")[idx],
            slot,
        )
        .node;
        self.release(&old);
        self.acquire(&new);
        self.changes.push(MasterChange {
            domain: domain.to_owned(),
            added: Some(new),
            removed: Some(old.clone()),
        });
        old
    }

    fn trim_surplus(&mut self) {
        for domain in self.domains() {
            while self.slots[&domain].len() > self.masters {
                let slot = self.slots.get_mut(&domain).and_then(Vec::pop);
                let Some(slot) = slot else { break };
                self.release(&slot.node);
                self.changes.push(MasterChange {
                    domain: domain.clone(),
                    added: None,
                    removed: Some(slot.node),
                });
            }
        }
    }

    fn replace_unhealthy(&mut self) {
        for domain in self.domains() {
            for idx in 0..self.slots[&domain].len() {
                if self.is_healthy(&self.slots[&domain][idx].node) {
                    continue;
                }
                if let Some(new) = self.pick(&domain, Some(idx)) {
                    self.swap(&domain, idx, new);
                }
            }
        }
    }

    fn fill_free(&mut self) {
        for domain in self.domains() {
            while self.slots[&domain].len() < self.masters {
                let Some(new) = self.pick(&domain, None) else {
                    break;
                };
                self.acquire(&new);
                self.changes.push(MasterChange {
                    domain: domain.clone(),
                    added: Some(new.clone()),
                    removed: None,
                });
                let slot = Slot {
                    node: new,
                    since: self.now,
                };
                self.slots
                    .get_mut(&domain)
                    .expect("known domain")
                    .push(slot);
            }
        }
    }

    /// One slot per domain per tick; 0 turns rotation off.
    fn rotate(&mut self, ttl_sec: u32) {
        if ttl_sec == 0 {
            return;
        }
        let ttl_ms = i64::from(ttl_sec) * 1000;
        for domain in self.domains() {
            for idx in 0..self.slots[&domain].len() {
                let slot = &self.slots[&domain][idx];
                let overdue = self.now.0.saturating_sub(slot.since.0) >= ttl_ms;
                if !overdue || !self.is_healthy(&slot.node) {
                    continue;
                }
                if let Some(new) = self.pick(&domain, Some(idx)) {
                    let old = self.swap(&domain, idx, new);
                    self.requeued.insert(old.clone(), self.now);
                    self.sort_queue();
                    if !self.rotated_out.contains(&old) {
                        self.rotated_out.push(old);
                    }
                    break;
                }
            }
        }
    }

    /// Every node without a domain takes one slot from the most loaded node. There is no full
    /// rebalancing: a node with two domains against another's one stays as it is.
    fn rebalance_idle(&mut self) {
        let idle: Vec<NodeId> = self
            .queue
            .iter()
            .filter(|n| self.load_of(&n.id) == 0)
            .map(|n| n.id.clone())
            .collect();
        for id in idle {
            if let Some((domain, idx)) = self.donor_slot(&id) {
                self.swap(&domain, idx, id);
            }
        }
    }

    /// The slot `idle` may take: from the most loaded donor (two domains or more), earliest in
    /// the queue among equals, the first domain by name where the IP does not clash.
    fn donor_slot(&self, idle: &NodeId) -> Option<(String, usize)> {
        let idle_ip = self.nodes.get(idle)?.ip;
        let mut donors: Vec<(usize, &NodeId)> = self
            .queue
            .iter()
            .map(|n| (self.load_of(&n.id), &n.id))
            .filter(|(load, _)| *load > 1)
            .collect();
        donors.sort_by_key(|(load, _)| std::cmp::Reverse(*load));
        for (_, donor) in donors {
            for (domain, slots) in &self.slots {
                let Some(idx) = slots.iter().position(|s| &s.node == donor) else {
                    continue;
                };
                if !self.ips_except(domain, Some(idx)).contains(&idle_ip) {
                    return Some((domain.clone(), idx));
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EngineKind, OwnerId};

    const T0: i64 = 1_800_000_000_000;

    fn at(sec: i64) -> Ts {
        Ts(T0 + sec * 1000)
    }

    fn nid(n: u32) -> NodeId {
        NodeId::parse(&format!("n-{n:05}")).unwrap()
    }

    /// A `Healthy` node queued at `queued` seconds.
    fn node(n: u32, addr: &str, queued: i64) -> NodeRecord {
        let mut r = NodeRecord::new(
            nid(n),
            OwnerId(1),
            addr.parse().unwrap(),
            443,
            EngineKind::Telemt,
            at(0),
            &Settings::default(),
        );
        r.state = NodeState::Healthy;
        r.queued_at = at(queued);
        r
    }

    /// Nodes `1..=count` on distinct public IPs, queued in id order.
    fn pool(count: u32) -> Vec<NodeRecord> {
        (1..=count)
            .map(|n| node(n, &format!("95.216.0.{n}"), i64::from(n)))
            .collect()
    }

    fn settings(masters: u32, ttl_sec: u32) -> Settings {
        Settings {
            masters_per_domain: masters,
            rotation_ttl_sec: ttl_sec,
            ..Settings::default()
        }
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|d| (*d).to_owned()).collect()
    }

    fn assigned(entries: &[(&str, &[(u32, i64)])]) -> Assignments {
        Assignments(
            entries
                .iter()
                .map(|(d, slots)| {
                    let slots = slots
                        .iter()
                        .map(|(n, since)| Slot {
                            node: nid(*n),
                            since: at(*since),
                        })
                        .collect();
                    ((*d).to_owned(), slots)
                })
                .collect(),
        )
    }

    /// The holders of a domain, in slot order.
    fn holders(a: &Assignments, domain: &str) -> Vec<NodeId> {
        a.0[domain].iter().map(|s| s.node.clone()).collect()
    }

    fn ids(list: &[u32]) -> Vec<NodeId> {
        list.iter().map(|n| nid(*n)).collect()
    }

    #[test]
    fn queue_order_uses_queued_registered_id() {
        let mut a = node(3, "95.216.0.3", 10);
        let mut b = node(2, "95.216.0.2", 10);
        let mut c = node(1, "95.216.0.1", 10);
        let early = node(4, "95.216.0.4", 5);
        let mut late = node(5, "95.216.0.5", 20);
        a.registered_at = at(1);
        b.registered_at = at(2);
        c.registered_at = at(2);
        late.registered_at = at(0);
        let mut quarantined = node(6, "95.216.0.6", 0);
        quarantined.state = NodeState::Quarantine;

        let order = queue_order(&[a, b, c, early, late, quarantined]);

        // queued_at first; equal queued_at by registered_at; then the id.
        assert_eq!(order, ids(&[4, 3, 1, 2, 5]));
    }

    #[test]
    fn fills_slot_with_least_loaded_then_queue() {
        // Node 1 masters two domains, node 2 one, nodes 3 and 4 none.
        let prev = assigned(&[
            ("a.example.com", &[(1, 0)]),
            ("b.example.com", &[(1, 0)]),
            ("c.example.com", &[(2, 0)]),
        ]);
        let domains = names(&[
            "a.example.com",
            "b.example.com",
            "c.example.com",
            "d.example.com",
            "e.example.com",
        ]);

        let out = select(&prev, &pool(4), &domains, at(60), &settings(1, 0));

        // Load first: 3 and 4 are free, tied, and 3 comes earlier in the queue. For the next
        // domain 4 has the lower load.
        assert_eq!(holders(&out.assignments, "d.example.com"), ids(&[3]));
        assert_eq!(holders(&out.assignments, "e.example.com"), ids(&[4]));
        assert_eq!(
            out.changes,
            vec![
                MasterChange {
                    domain: "d.example.com".into(),
                    added: Some(nid(3)),
                    removed: None,
                },
                MasterChange {
                    domain: "e.example.com".into(),
                    added: Some(nid(4)),
                    removed: None,
                },
            ]
        );
        assert_eq!(out.assignments.0["d.example.com"][0].since, at(60));
    }

    #[test]
    fn same_ip_never_twice_on_domain() {
        let nodes = vec![
            node(1, "95.216.0.1", 1),
            node(2, "95.216.0.1", 2),
            node(3, "95.216.0.3", 3),
        ];
        let domains = names(&["a.example.com"]);

        let out = select(
            &Assignments::default(),
            &nodes,
            &domains,
            at(0),
            &settings(2, 0),
        );
        assert_eq!(holders(&out.assignments, "a.example.com"), ids(&[1, 3]));

        // Without a node on another IP the second slot stays free.
        let out = select(
            &Assignments::default(),
            &nodes[..2],
            &domains,
            at(0),
            &settings(2, 0),
        );
        assert_eq!(holders(&out.assignments, "a.example.com"), ids(&[1]));
    }

    #[test]
    fn ttl_rotation_one_slot_per_domain_per_tick() {
        // Both slots of both domains are overdue. Nodes 5 and 6 are free.
        let prev = assigned(&[
            ("a.example.com", &[(1, 0), (2, 0)]),
            ("b.example.com", &[(3, 0), (4, 0)]),
        ]);
        let domains = names(&["a.example.com", "b.example.com"]);

        let out = select(&prev, &pool(6), &domains, at(1800), &settings(2, 1800));

        for domain in ["a.example.com", "b.example.com"] {
            let swapped = out.assignments.0[domain]
                .iter()
                .filter(|s| s.since == at(1800))
                .count();
            assert_eq!(swapped, 1, "{domain}");
        }
        assert_eq!(out.changes.len(), 2);
        // The first overdue slot goes, the other keeps its time.
        assert_eq!(holders(&out.assignments, "a.example.com"), ids(&[5, 2]));
        assert_eq!(out.assignments.0["a.example.com"][1].since, at(0));
        assert_eq!(holders(&out.assignments, "b.example.com"), ids(&[6, 4]));
        assert_eq!(out.rotated_out, ids(&[1, 3]));

        // The next tick takes the other slot.
        let out = select(
            &out.assignments,
            &pool(6),
            &domains,
            at(1803),
            &settings(2, 1800),
        );
        assert_eq!(out.changes.len(), 2);
        assert_eq!(out.assignments.0["a.example.com"][0].since, at(1800));
        assert_eq!(out.assignments.0["a.example.com"][1].since, at(1803));
    }

    #[test]
    fn slot_younger_than_ttl_stays() {
        let prev = assigned(&[("a.example.com", &[(1, 1)])]);
        let domains = names(&["a.example.com"]);

        let out = select(&prev, &pool(2), &domains, at(1800), &settings(1, 1800));
        assert_eq!(out.assignments, prev);
        assert!(out.changes.is_empty() && out.rotated_out.is_empty());

        let out = select(&prev, &pool(2), &domains, at(1801), &settings(1, 1800));
        assert_eq!(holders(&out.assignments, "a.example.com"), ids(&[2]));
    }

    #[test]
    fn rotation_zero_disables() {
        let prev = assigned(&[("a.example.com", &[(1, 0)])]);
        let domains = names(&["a.example.com"]);

        let out = select(&prev, &pool(3), &domains, at(1_000_000), &settings(1, 0));

        assert_eq!(out.assignments, prev);
        assert!(out.changes.is_empty());
        assert!(out.rotated_out.is_empty());
    }

    #[test]
    fn rotation_without_replacement_keeps_the_slot() {
        // The only other node is not Healthy, and the third shares the master's IP.
        let mut quarantined = node(2, "95.216.0.2", 2);
        quarantined.state = NodeState::Quarantine;
        let nodes = vec![node(1, "95.216.0.1", 1), quarantined];
        let prev = assigned(&[("a.example.com", &[(1, 0)])]);
        let domains = names(&["a.example.com"]);

        let out = select(&prev, &nodes, &domains, at(9999), &settings(1, 60));

        assert_eq!(out.assignments, prev);
        assert!(out.rotated_out.is_empty());
    }

    #[test]
    fn rotated_node_goes_to_tail() {
        // Nodes 1 and 2 are overdue on their domains, nodes 3 and 4 hold nothing.
        let prev = assigned(&[("a.example.com", &[(1, 0)]), ("b.example.com", &[(2, 0)])]);
        let domains = names(&["a.example.com", "b.example.com"]);
        let mut nodes = pool(4);

        let out = select(&prev, &nodes, &domains, at(100), &settings(1, 60));

        // Node 1 left `a`, node 3 took it. For `b` nodes 1 and 4 both have no domain, yet
        // node 1 is now behind node 4 in the queue.
        assert_eq!(holders(&out.assignments, "a.example.com"), ids(&[3]));
        assert_eq!(holders(&out.assignments, "b.example.com"), ids(&[4]));
        assert_eq!(out.rotated_out, ids(&[1, 2]));

        // The caller moves the rotated nodes to the back of the queue.
        for n in &mut nodes {
            if out.rotated_out.contains(&n.id) {
                n.queued_at = at(100);
            }
        }
        assert_eq!(queue_order(&nodes), ids(&[3, 4, 1, 2]));
    }

    #[test]
    fn removed_master_replaced_same_tick() {
        let mut gone = node(1, "95.216.0.1", 1);
        gone.state = NodeState::Quarantine;
        let mut nodes = pool(3);
        nodes[0] = gone;
        let prev = assigned(&[("a.example.com", &[(1, 0), (2, 0)])]);
        let domains = names(&["a.example.com"]);

        let out = select(&prev, &nodes, &domains, at(30), &settings(2, 0));

        assert_eq!(holders(&out.assignments, "a.example.com"), ids(&[3, 2]));
        assert_eq!(out.assignments.0["a.example.com"][0].since, at(30));
        assert_eq!(out.assignments.0["a.example.com"][1].since, at(0));
        assert_eq!(
            out.changes,
            vec![MasterChange {
                domain: "a.example.com".into(),
                added: Some(nid(3)),
                removed: Some(nid(1)),
            }]
        );

        // A master that is no longer in the pool at all is replaced the same way.
        let out = select(&prev, &pool(3)[1..], &domains, at(30), &settings(2, 0));
        assert_eq!(holders(&out.assignments, "a.example.com"), ids(&[3, 2]));
    }

    #[test]
    fn empty_queue_keeps_assignments() {
        let mut down = node(1, "95.216.0.1", 1);
        down.state = NodeState::Quarantine;
        let prev = assigned(&[("a.example.com", &[(1, 0)])]);
        let domains = names(&["a.example.com", "b.example.com"]);

        // Rotation is due too, but there is nobody to take over.
        let out = select(&prev, &[down], &domains, at(99_999), &settings(1, 60));

        assert_eq!(holders(&out.assignments, "a.example.com"), ids(&[1]));
        assert_eq!(out.assignments.0["a.example.com"][0].since, at(0));
        assert!(out.assignments.0["b.example.com"].is_empty());
        assert!(out.changes.is_empty());
        assert!(out.rotated_out.is_empty());

        let out = select(&prev, &[], &domains, at(99_999), &settings(1, 60));
        assert_eq!(holders(&out.assignments, "a.example.com"), ids(&[1]));
    }

    #[test]
    fn node_without_domain_takes_one_slot_from_loaded_donor() {
        // Node 1 masters three domains and node 2 none.
        let prev = assigned(&[
            ("a.example.com", &[(1, 0)]),
            ("b.example.com", &[(1, 5)]),
            ("c.example.com", &[(1, 7)]),
        ]);
        let domains = names(&["a.example.com", "b.example.com", "c.example.com"]);

        let out = select(&prev, &pool(2), &domains, at(60), &settings(1, 0));

        assert_eq!(holders(&out.assignments, "a.example.com"), ids(&[2]));
        assert_eq!(out.assignments.0["a.example.com"][0].since, at(60));
        // One slot only; the rest is not rebalanced.
        assert_eq!(holders(&out.assignments, "b.example.com"), ids(&[1]));
        assert_eq!(out.assignments.0["b.example.com"][0].since, at(5));
        assert_eq!(holders(&out.assignments, "c.example.com"), ids(&[1]));
        assert_eq!(
            out.changes,
            vec![MasterChange {
                domain: "a.example.com".into(),
                added: Some(nid(2)),
                removed: Some(nid(1)),
            }]
        );
    }

    #[test]
    fn two_against_one_is_not_rebalanced() {
        let prev = assigned(&[
            ("a.example.com", &[(1, 0)]),
            ("b.example.com", &[(1, 0)]),
            ("c.example.com", &[(2, 0)]),
        ]);
        let domains = names(&["a.example.com", "b.example.com", "c.example.com"]);

        let out = select(&prev, &pool(2), &domains, at(60), &settings(1, 0));

        assert_eq!(out.assignments, prev);
    }

    #[test]
    fn masters_limit_respected() {
        let domains = names(&["a.example.com", "b.example.com"]);

        let out = select(
            &Assignments::default(),
            &pool(5),
            &domains,
            at(0),
            &settings(2, 0),
        );
        for domain in &domains {
            assert_eq!(out.assignments.0[domain].len(), 2, "{domain}");
        }

        // The limit was lowered: the surplus slots go, the first ones stay.
        let out = select(&out.assignments, &pool(5), &domains, at(3), &settings(1, 0));
        for domain in &domains {
            assert_eq!(out.assignments.0[domain].len(), 1, "{domain}");
        }
        assert_eq!(out.changes.len(), 2);
        assert!(
            out.changes
                .iter()
                .all(|c| c.added.is_none() && c.removed.is_some())
        );
    }

    #[test]
    fn removed_domain_drops_out_and_new_one_starts_empty() {
        let prev = assigned(&[("old.example.com", &[(1, 0)])]);
        let domains = names(&["new.example.com"]);

        let out = select(&prev, &[], &domains, at(0), &settings(1, 0));

        assert_eq!(
            out.assignments.0.keys().collect::<Vec<_>>(),
            ["new.example.com"]
        );
        assert!(out.assignments.0["new.example.com"].is_empty());
        assert!(out.changes.is_empty());
    }

    #[test]
    fn output_is_deterministic() {
        let prev = assigned(&[("a.example.com", &[(1, 0)]), ("b.example.com", &[(1, 0)])]);
        let domains = names(&["b.example.com", "a.example.com", "c.example.com"]);
        let nodes = pool(4);

        let first = select(&prev, &nodes, &domains, at(3600), &settings(2, 60));
        let again = select(&prev, &nodes, &domains, at(3600), &settings(2, 60));
        let mut reversed = nodes.clone();
        reversed.reverse();
        let shuffled = select(&prev, &reversed, &domains, at(3600), &settings(2, 60));

        assert_eq!(first, again);
        assert_eq!(first, shuffled);
    }
}
