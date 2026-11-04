//! Read snapshots: immutable, with the JSON already rendered, swapped in by the state owner.
//!
//! Readers call `SnapshotStore::load` and never talk to the actor.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use arc_swap::ArcSwap;
use mtpool_core::{
    EngineKind, GpStatus, NodeId, NodeRecord, NodeState as CoreState, PoolState, RecordValue,
    build_proxy_link, mask_ip, next_round_at, queue_positions,
};
use mtpool_protocol::public::{
    DnsRecord, DnsRecordType, Engine, GlobalpingCheck, NodeChecks, NodeState, ProxyLink,
    PublicDomain, PublicNode,
};
use mtpool_protocol::rfc3339;

/// One published view of the pool.
///
/// The documents are the public API ones (`PublicNode`, `PublicDomain`, `ProxyLink` lists),
/// rendered once per change; the HTTP handlers only hand the strings out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub nodes_json: String,
    pub domains_json: String,
    pub links_json: String,
    /// Changes whenever any of the three documents changes: 16 hex digits of FNV-1a.
    pub etag: String,
    /// Every node by id, banned ones too, which `nodes_json` leaves out (without `last_round`).
    pub node_docs: BTreeMap<String, PublicNode>,
    /// The Telegram link of each `ProxyLink::id`, for the QR codes.
    pub link_index: BTreeMap<String, String>,
    /// How many masters each configured domain has now (0 for a domain without one).
    pub master_counts: BTreeMap<String, usize>,
}

fn node_state(state: CoreState) -> NodeState {
    match state {
        CoreState::Pending => NodeState::Pending,
        CoreState::Healthy => NodeState::Healthy,
        CoreState::Quarantine => NodeState::Quarantine,
        CoreState::Banned => NodeState::Banned,
        CoreState::Dead => NodeState::Dead,
        CoreState::Pruned => NodeState::Pruned,
    }
}

fn engine(kind: EngineKind) -> Engine {
    match kind {
        EngineKind::Telemt => Engine::Telemt,
        EngineKind::MtProxyL => Engine::MtProxyL,
        EngineKind::Meko => Engine::Meko,
    }
}

/// Percent-encodes everything but the characters a path segment keeps as they are.
fn encode_segment(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'@') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// What the node documents need besides the node itself, worked out once per snapshot (or once
/// per request that shows several nodes).
pub(crate) struct View<'a> {
    state: &'a PoolState,
    masters_of: HashMap<&'a str, Vec<String>>,
    /// The earliest slot start of each master.
    since_of: HashMap<&'a str, i64>,
    queue: BTreeMap<NodeId, u32>,
}

impl<'a> View<'a> {
    pub(crate) fn new(state: &'a PoolState) -> View<'a> {
        let mut masters_of: HashMap<&str, Vec<String>> = HashMap::new();
        let mut since_of: HashMap<&str, i64> = HashMap::new();
        for domain in &state.settings.domains {
            for slot in state.assignments.0.get(domain).into_iter().flatten() {
                masters_of
                    .entry(slot.node.as_str())
                    .or_default()
                    .push(domain.clone());
                since_of
                    .entry(slot.node.as_str())
                    .and_modify(|at| *at = (*at).min(slot.since.0))
                    .or_insert(slot.since.0);
            }
        }
        View {
            state,
            masters_of,
            since_of,
            queue: queue_positions(state.nodes.values(), &state.assignments),
        }
    }

    /// One node as the public API shows it, with the domains it is master of now.
    pub(crate) fn node(&self, n: &NodeRecord) -> PublicNode {
        let domains = self
            .masters_of
            .get(n.id.as_str())
            .cloned()
            .unwrap_or_default();
        let settings = &self.state.settings;
        let ttl_ms = i64::from(settings.rotation_ttl_sec) * 1000;
        let rotates_at = match self.since_of.get(n.id.as_str()) {
            Some(since) if ttl_ms > 0 => Some(rfc3339(since.saturating_add(ttl_ms))),
            _ => None,
        };
        let stats = self.state.stats.get(&n.id);
        PublicNode {
            id: n.id.as_str().to_owned(),
            state: node_state(n.state),
            engine: engine(n.engine),
            ip: mask_ip(n.ip),
            port: n.port,
            registered_at: rfc3339(n.registered_at.0),
            state_since: rfc3339(n.state_since.0),
            is_master: !domains.is_empty(),
            domains,
            clients: stats.and_then(|s| s.clients),
            other_clients: stats.and_then(|s| s.other_clients).filter(|n| *n > 0),
            other_traffic_bytes: stats
                .map(|s| {
                    s.other_from_client_total
                        .saturating_add(s.other_to_client_total)
                })
                .filter(|n| *n > 0),
            checks: NodeChecks {
                tcp: n.health.tcp.up,
                metrics: n.health.metrics.up,
                globalping: match n.health.gp {
                    GpStatus::Unknown => GlobalpingCheck::Unknown,
                    GpStatus::Ok { .. } => GlobalpingCheck::Ok,
                    GpStatus::Failed { .. } => GlobalpingCheck::Failed,
                    GpStatus::NoData { .. } => GlobalpingCheck::NoData,
                },
            },
            quarantine_reasons: n
                .quarantine_reasons
                .iter()
                .filter_map(|c| match serde_json::to_value(c) {
                    Ok(serde_json::Value::String(name)) => Some(name),
                    _ => None,
                })
                .collect(),
            queue_position: self.queue.get(&n.id).copied(),
            master_since: self.since_of.get(n.id.as_str()).map(|at| rfc3339(*at)),
            rotates_at,
            next_round_at: next_round_at(n, settings).map(|at| rfc3339(at.0)),
            availability_bp: stats.and_then(|s| s.availability_bp),
            last_round: None,
        }
    }
}

impl Snapshot {
    pub fn build(state: &PoolState) -> Snapshot {
        let settings = &state.settings;

        let view = View::new(state);
        let nodes: Vec<PublicNode> = state.nodes.values().map(|n| view.node(n)).collect();

        let domains: Vec<PublicDomain> = settings
            .domains
            .iter()
            .map(|domain| PublicDomain {
                domain: domain.clone(),
                masters: state
                    .assignments
                    .0
                    .get(domain)
                    .map(|slots| slots.iter().map(|s| s.node.as_str().to_owned()).collect())
                    .unwrap_or_default(),
                dns: state.dns.get(domain).map(|d| match &d.desired {
                    RecordValue::A(ips) => DnsRecord {
                        kind: DnsRecordType::A,
                        values: ips.iter().map(|ip| mask_ip(*ip)).collect(),
                    },
                    RecordValue::Cname(target) => DnsRecord {
                        kind: DnsRecordType::Cname,
                        values: vec![target.clone()],
                    },
                }),
                clients: state.srmd.domain_clients.get(domain).copied(),
                folded_into: state.srmd.cnames.get(domain).cloned(),
                // A domain with no DNS entry has nothing in DNS to match.
                dns_synced: state
                    .dns
                    .get(domain)
                    .is_some_and(|d| d.applied.as_ref() == Some(&d.desired)),
            })
            .collect();

        let mut links = Vec::new();
        let mut link_index = BTreeMap::new();
        for domain in &settings.domains {
            for user in &settings.proxy_users {
                let id = format!("{}@{domain}", user.name);
                let link = build_proxy_link(
                    domain,
                    settings.proxy_port,
                    &user.secret_hex,
                    &settings.tls_domain,
                );
                link_index.insert(id.clone(), link.clone());
                links.push(ProxyLink {
                    qr_url: format!("/api/v1/links/{}/qr.svg", encode_segment(&id)),
                    id,
                    domain: domain.clone(),
                    user: user.name.clone(),
                    link,
                });
            }
        }

        // A banned node is not part of the pool any more: it stays in the ban history, and its
        // page opens from there, but the list does not show it.
        let listed: Vec<&PublicNode> = nodes
            .iter()
            .filter(|n| !matches!(n.state, NodeState::Banned))
            .collect();
        let nodes_json = render(&listed);
        let domains_json = render(&domains);
        let links_json = render(&links);
        let etag = etag_of(&[&nodes_json, &domains_json, &links_json]);
        Snapshot {
            nodes_json,
            domains_json,
            links_json,
            etag,
            node_docs: nodes.into_iter().map(|n| (n.id.clone(), n)).collect(),
            link_index,
            master_counts: settings
                .domains
                .iter()
                .map(|d| (d.clone(), state.assignments.0.get(d).map_or(0, Vec::len)))
                .collect(),
        }
    }
}

fn render<T: serde::Serialize>(documents: &[T]) -> String {
    serde_json::to_string(documents).expect("public documents serialize")
}

pub(crate) fn etag_of(parts: &[&str]) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for part in parts {
        for byte in part.bytes().chain(std::iter::once(0)) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(PRIME);
        }
    }
    format!("{hash:016x}")
}

/// The published snapshot. Clones share it.
#[derive(Clone)]
pub struct SnapshotStore {
    current: Arc<ArcSwap<Snapshot>>,
}

impl SnapshotStore {
    pub fn new(initial: Snapshot) -> SnapshotStore {
        SnapshotStore {
            current: Arc::new(ArcSwap::from_pointee(initial)),
        }
    }

    pub fn load(&self) -> Arc<Snapshot> {
        self.current.load_full()
    }

    /// Replaces the snapshot; an identical one (same etag) is not swapped. Returns whether it
    /// was.
    pub fn publish(&self, next: Snapshot) -> bool {
        if self.current.load().etag == next.etag {
            return false;
        }
        self.current.store(Arc::new(next));
        true
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use mtpool_core::{
        DnsState, EngineKind, Generation, NodeId, NodeRecord, NodeState, OwnerId, PoolState,
        RecordValue, Settings, Slot, Ts,
    };

    use super::*;

    const T0: i64 = 1_700_000_000_000;

    fn node(name: &str, registered: i64) -> NodeRecord {
        let mut n = NodeRecord::new(
            NodeId::parse(name).unwrap(),
            OwnerId(1),
            Ipv4Addr::new(203, 0, 113, 7),
            443,
            EngineKind::Telemt,
            Ts(registered),
            &Settings::default(),
        );
        n.state = NodeState::Healthy;
        n.queued_at = Ts(registered);
        n
    }

    fn pool(settings: Settings, nodes: Vec<NodeRecord>) -> PoolState {
        let mut state = PoolState::new(settings);
        for n in nodes {
            state.nodes.insert(n.id.clone(), n);
        }
        state
    }

    fn settings(ttl: u32) -> Settings {
        Settings {
            domains: vec!["a.example.com".into(), "b.example.com".into()],
            rotation_ttl_sec: ttl,
            ..Settings::default()
        }
    }

    fn make_master(state: &mut PoolState, domain: &str, name: &str, since: i64) {
        state
            .assignments
            .0
            .entry(domain.to_owned())
            .or_default()
            .push(Slot {
                node: NodeId::parse(name).unwrap(),
                since: Ts(since),
            });
    }

    #[test]
    fn the_owners_users_show_only_when_there_is_something_to_show() {
        let n = node("alpha-00001", T0);
        let id = n.id.clone();
        let mut state = pool(settings(0), vec![n]);
        let stats = state.stats.entry(id.clone()).or_default();
        stats.other_clients = Some(0);
        let doc = Snapshot::build(&state).node_docs["alpha-00001"].clone();
        assert_eq!((doc.other_clients, doc.other_traffic_bytes), (None, None));
        let json = serde_json::to_value(&doc).unwrap();
        assert!(json.get("other_clients").is_none());
        assert!(json.get("other_traffic_bytes").is_none());

        let stats = state.stats.entry(id).or_default();
        stats.other_clients = Some(7);
        stats.other_from_client_total = 100;
        stats.other_to_client_total = 900;
        let doc = Snapshot::build(&state).node_docs["alpha-00001"].clone();
        assert_eq!(
            (doc.other_clients, doc.other_traffic_bytes),
            (Some(7), Some(1000))
        );
    }

    #[test]
    fn a_node_carries_its_counters_and_checks() {
        let mut n = node("alpha-00001", T0);
        n.health.tcp.up = true;
        n.health.metrics.up = false;
        n.health.gp = GpStatus::NoData { at: Ts(T0) };
        let id = n.id.clone();
        let mut state = pool(settings(0), vec![n]);
        let stats = state.stats.entry(id).or_default();
        stats.clients = Some(12);
        stats.availability_bp = Some(9997);
        let snapshot = Snapshot::build(&state);
        let doc = &snapshot.node_docs["alpha-00001"];
        assert_eq!(doc.clients, Some(12));
        assert_eq!(doc.availability_bp, Some(9997));
        assert_eq!(doc.other_clients, None);
        assert_eq!(doc.other_traffic_bytes, None);
        assert!(doc.checks.tcp);
        assert!(!doc.checks.metrics);
        assert_eq!(doc.checks.globalping, GlobalpingCheck::NoData);
        assert_eq!(doc.next_round_at, None, "a healthy node runs no rounds");
    }

    #[test]
    fn the_queue_numbers_waiting_nodes_and_leaves_masters_out() {
        let mut state = pool(
            settings(0),
            vec![
                node("alpha-00001", T0),
                node("alpha-00002", T0 + 1),
                node("alpha-00003", T0 + 2),
            ],
        );
        make_master(&mut state, "a.example.com", "alpha-00001", T0);
        let snapshot = Snapshot::build(&state);
        let docs = &snapshot.node_docs;
        assert_eq!(docs["alpha-00001"].queue_position, None);
        assert_eq!(docs["alpha-00002"].queue_position, Some(1));
        assert_eq!(docs["alpha-00003"].queue_position, Some(2));
        let third = &state.nodes[&NodeId::parse("alpha-00003").unwrap()];
        assert_eq!(View::new(&state).node(third).queue_position, Some(2));
    }

    #[test]
    fn a_master_rotates_at_its_earliest_slot_plus_the_ttl() {
        let mut state = pool(settings(600), vec![node("alpha-00001", T0)]);
        make_master(&mut state, "a.example.com", "alpha-00001", T0 + 5_000);
        make_master(&mut state, "b.example.com", "alpha-00001", T0 + 1_000);
        let snapshot = Snapshot::build(&state);
        let doc = &snapshot.node_docs["alpha-00001"];
        assert_eq!(doc.rotates_at, Some(rfc3339(T0 + 1_000 + 600_000)));
    }

    #[test]
    fn a_master_shows_since_when_it_has_been_master() {
        let mut state = pool(
            settings(600),
            vec![node("alpha-00001", T0), node("alpha-00002", T0)],
        );
        make_master(&mut state, "a.example.com", "alpha-00001", T0 + 5_000);
        make_master(&mut state, "b.example.com", "alpha-00001", T0 + 1_000);
        let snapshot = Snapshot::build(&state);
        assert_eq!(
            snapshot.node_docs["alpha-00001"].master_since,
            Some(rfc3339(T0 + 1_000))
        );
        let other = &snapshot.node_docs["alpha-00002"];
        assert_eq!(other.master_since, None);
        let json = serde_json::to_value(other).unwrap();
        assert!(json.get("master_since").is_none());
    }

    #[test]
    fn without_rotation_a_master_has_no_rotation_time() {
        let mut state = pool(settings(0), vec![node("alpha-00001", T0)]);
        make_master(&mut state, "a.example.com", "alpha-00001", T0);
        let snapshot = Snapshot::build(&state);
        let doc = &snapshot.node_docs["alpha-00001"];
        assert!(doc.is_master);
        assert_eq!(doc.rotates_at, None);
        let json = serde_json::to_value(doc).unwrap();
        assert!(json.get("rotates_at").is_none());
    }

    #[test]
    fn a_quarantined_node_shows_when_its_next_round_is_due() {
        let mut n = node("alpha-00001", T0);
        n.state = NodeState::Quarantine;
        n.state_since = Ts(T0 + 3_000);
        let state = pool(settings(0), vec![n]);
        let snapshot = Snapshot::build(&state);
        assert_eq!(
            snapshot.node_docs["alpha-00001"].next_round_at,
            Some(rfc3339(T0 + 3_000))
        );
    }

    #[test]
    fn a_domain_shows_clients_fold_and_dns_sync() {
        let mut state = pool(settings(0), vec![]);
        let ip = Ipv4Addr::new(203, 0, 113, 7);
        let dns = |applied: Option<RecordValue>| DnsState {
            desired: RecordValue::A(vec![ip]),
            applied,
            generation: Generation(1),
            retries: 0,
            next_try: Ts(T0),
            in_flight: None,
        };
        state
            .dns
            .insert("a.example.com".into(), dns(Some(RecordValue::A(vec![ip]))));
        state.dns.insert("b.example.com".into(), dns(None));
        state.srmd.domain_clients.insert("a.example.com".into(), 40);
        state
            .srmd
            .cnames
            .insert("b.example.com".into(), "a.example.com".into());
        let snapshot = Snapshot::build(&state);
        let domains: Vec<PublicDomain> = serde_json::from_str(&snapshot.domains_json).unwrap();
        assert_eq!(domains[0].clients, Some(40));
        assert_eq!(domains[0].folded_into, None);
        assert!(domains[0].dns_synced);
        assert_eq!(domains[1].clients, None);
        assert_eq!(domains[1].folded_into.as_deref(), Some("a.example.com"));
        assert!(!domains[1].dns_synced, "nothing applied yet");
    }

    #[test]
    fn a_domain_without_a_dns_entry_is_not_synced() {
        let state = pool(settings(0), vec![]);
        let snapshot = Snapshot::build(&state);
        let domains: Vec<PublicDomain> = serde_json::from_str(&snapshot.domains_json).unwrap();
        assert!(domains.iter().all(|d| !d.dns_synced));
    }
}
