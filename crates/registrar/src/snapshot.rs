//! Read snapshots: immutable, with the JSON already rendered, swapped in by the state owner.
//!
//! Readers call `SnapshotStore::load` and never talk to the actor.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use arc_swap::ArcSwap;
use mtpool_core::{
    EngineKind, NodeRecord, NodeState as CoreState, PoolState, RecordValue, build_proxy_link,
    mask_ip,
};
use mtpool_protocol::public::{
    DnsRecord, DnsRecordType, Engine, NodeState, ProxyLink, PublicDomain, PublicNode,
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
    /// The nodes by id, as in `nodes_json` (without `last_round`).
    pub node_docs: BTreeMap<String, PublicNode>,
    /// The Telegram link of each `ProxyLink::id`, for the QR codes.
    pub link_index: BTreeMap<String, String>,
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

fn public_node(n: &NodeRecord, masters_of: &HashMap<&str, Vec<String>>) -> PublicNode {
    let domains = masters_of.get(n.id.as_str()).cloned().unwrap_or_default();
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
        last_round: None,
    }
}

impl Snapshot {
    pub fn build(state: &PoolState) -> Snapshot {
        let settings = &state.settings;

        let mut masters_of: HashMap<&str, Vec<String>> = HashMap::new();
        for domain in &settings.domains {
            for slot in state.assignments.0.get(domain).into_iter().flatten() {
                masters_of
                    .entry(slot.node.as_str())
                    .or_default()
                    .push(domain.clone());
            }
        }
        let nodes: Vec<PublicNode> = state
            .nodes
            .values()
            .map(|n| public_node(n, &masters_of))
            .collect();

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

        let nodes_json = render(&nodes);
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
