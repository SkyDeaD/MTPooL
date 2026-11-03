//! Read snapshots: immutable, with the JSON already rendered, swapped in by the state owner.
//!
//! Readers call `SnapshotStore::load` and never talk to the actor.

use std::sync::Arc;

use arc_swap::ArcSwap;
use mtpool_core::{PoolState, RecordValue, build_proxy_link, mask_ip};
use serde_json::{Value, json};

/// One published view of the pool.
///
/// The JSON is an interim shape (masked node addresses, masters, desired DNS, Telegram links);
/// the public API contract fixes the final one together with the HTTP routes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub nodes_json: String,
    pub domains_json: String,
    pub links_json: String,
    /// Changes whenever any of the three documents changes: 16 hex digits of FNV-1a.
    pub etag: String,
}

impl Snapshot {
    pub fn build(state: &PoolState) -> Snapshot {
        let nodes: Vec<Value> = state
            .nodes
            .values()
            .map(|n| {
                json!({
                    "id": n.id.as_str(),
                    "state": n.state,
                    "engine": n.engine,
                    "ip": mask_ip(n.ip),
                    "registered_at": n.registered_at,
                })
            })
            .collect();

        let domains: Vec<Value> = state
            .settings
            .domains
            .iter()
            .map(|domain| {
                let masters: Vec<&str> = state
                    .assignments
                    .0
                    .get(domain)
                    .map(|slots| slots.iter().map(|s| s.node.as_str()).collect())
                    .unwrap_or_default();
                let records = state.dns.get(domain).map(|d| match &d.desired {
                    RecordValue::A(ips) => json!({
                        "type": "A",
                        "values": ips.iter().map(ToString::to_string).collect::<Vec<_>>(),
                    }),
                    RecordValue::Cname(target) => json!({ "type": "CNAME", "values": [target] }),
                });
                json!({ "domain": domain, "masters": masters, "dns": records })
            })
            .collect();

        let settings = &state.settings;
        let links: Vec<Value> = settings
            .domains
            .iter()
            .flat_map(|domain| {
                settings.proxy_users.iter().map(move |user| {
                    json!({
                        "domain": domain,
                        "user": user.name,
                        "link": build_proxy_link(
                            domain,
                            settings.proxy_port,
                            &user.secret_hex,
                            &settings.tls_domain,
                        ),
                    })
                })
            })
            .collect();

        let nodes_json = Value::Array(nodes).to_string();
        let domains_json = Value::Array(domains).to_string();
        let links_json = Value::Array(links).to_string();
        let etag = etag_of(&[&nodes_json, &domains_json, &links_json]);
        Snapshot {
            nodes_json,
            domains_json,
            links_json,
            etag,
        }
    }
}

fn etag_of(parts: &[&str]) -> String {
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
