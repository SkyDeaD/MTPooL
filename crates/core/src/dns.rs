//! DNS change planning for one domain. The registrar's worker executes the operations.

use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::net::Ipv4Addr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::types::{Generation, Ts};

/// What a domain should resolve to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordValue {
    A(Vec<Ipv4Addr>),
    Cname(String),
}

/// Desired/applied DNS state of one domain. `generation` is checked before a worker
/// result is applied, so a stale result is rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsState {
    pub desired: RecordValue,
    pub applied: Option<RecordValue>,
    pub generation: Generation,
    pub retries: u32,
    pub next_try: Ts,
    /// The generation a worker is pushing right now. Memory of the running process only: a
    /// restart forgets it, so a push lost with the process is tried again.
    #[serde(skip)]
    pub in_flight: Option<Generation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordKind {
    A,
    Aaaa,
    Cname,
    Other(String),
}

/// A record as the provider returned it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveRecord {
    pub id: String,
    pub name: String,
    pub kind: RecordKind,
    pub content: String,
    pub proxied: bool,
}

/// A record to create. The worker adds ttl and proxied from the settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRecord {
    pub name: String,
    pub kind: RecordKind,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DnsOp {
    Create(NewRecord),
    /// Provider record id.
    Delete(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DnsPlanError {
    /// An empty desired state is never applied.
    EmptyDesired,
    /// A record the pool did not create blocks a required type change.
    UnmanagedConflict { record_id: String },
}

impl fmt::Display for DnsPlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DnsPlanError::EmptyDesired => write!(f, "desired DNS state is empty"),
            DnsPlanError::UnmanagedConflict { record_id } => {
                write!(f, "unmanaged record {record_id} blocks the change")
            }
        }
    }
}

impl std::error::Error for DnsPlanError {}

/// Plans the operations that move one domain from `live` to `desired`.
///
/// `live` holds every record the provider returned for `name`, of any kind. `managed` holds
/// the ids of records the pool created. Order: records whose type blocks the new one, then
/// creates, then leftover managed records, so the domain is never empty except when the
/// provider forces a type change. Only managed A, AAAA and CNAME records are deleted.
pub fn plan_changes(
    name: &str,
    desired: &RecordValue,
    live: &[LiveRecord],
    managed: &HashSet<String>,
) -> Result<Vec<DnsOp>, DnsPlanError> {
    let (kind, wanted) = match desired {
        RecordValue::A(ips) => (
            RecordKind::A,
            ips.iter()
                .copied()
                .collect::<BTreeSet<_>>()
                .iter()
                .map(Ipv4Addr::to_string)
                .collect(),
        ),
        RecordValue::Cname(target) => (RecordKind::Cname, vec![normalize_cname(target)]),
    };
    if wanted.is_empty() || wanted.iter().any(String::is_empty) {
        return Err(DnsPlanError::EmptyDesired);
    }

    let mut records: Vec<&LiveRecord> = live.iter().collect();
    records.sort_by(|a, b| a.id.cmp(&b.id));

    let mut present: HashSet<&str> = HashSet::new();
    let mut blocking = Vec::new();
    let mut leftovers = Vec::new();
    for record in records {
        if record.kind == kind
            && let Some(value) = wanted
                .iter()
                .find(|w| same_value(&kind, w, &record.content))
        {
            present.insert(value);
            continue;
        }
        let is_managed = managed.contains(&record.id);
        if blocks(&kind, &record.kind) {
            if !is_managed {
                return Err(DnsPlanError::UnmanagedConflict {
                    record_id: record.id.clone(),
                });
            }
            blocking.push(record.id.clone());
        } else if is_managed && is_deletable(&record.kind) {
            leftovers.push(record.id.clone());
        }
    }

    let mut ops: Vec<DnsOp> = blocking.into_iter().map(DnsOp::Delete).collect();
    ops.extend(
        wanted
            .iter()
            .filter(|w| !present.contains(w.as_str()))
            .map(|content| {
                DnsOp::Create(NewRecord {
                    name: name.to_string(),
                    kind: kind.clone(),
                    content: content.clone(),
                })
            }),
    );
    ops.extend(leftovers.into_iter().map(DnsOp::Delete));
    Ok(ops)
}

fn normalize_cname(target: &str) -> String {
    target.trim_end_matches('.').to_ascii_lowercase()
}

fn same_value(kind: &RecordKind, wanted: &str, live: &str) -> bool {
    match kind {
        RecordKind::Cname => wanted == normalize_cname(live),
        _ => wanted == live,
    }
}

/// Cloudflare holds a CNAME alone at a name: it cannot sit next to an A or AAAA record or
/// another CNAME. An A set only conflicts with a CNAME.
fn blocks(desired: &RecordKind, live: &RecordKind) -> bool {
    match desired {
        RecordKind::A => *live == RecordKind::Cname,
        RecordKind::Cname => matches!(live, RecordKind::A | RecordKind::Aaaa | RecordKind::Cname),
        _ => false,
    }
}

fn is_deletable(kind: &RecordKind) -> bool {
    matches!(kind, RecordKind::A | RecordKind::Aaaa | RecordKind::Cname)
}

/// 5 s doubling with every retry, capped at 5 minutes.
pub fn backoff(retries: u32) -> Duration {
    const BASE_SECS: u64 = 5;
    const CAP_SECS: u64 = 300;
    // 5 << 6 already exceeds the cap, so larger shifts never need computing.
    Duration::from_secs((BASE_SECS << retries.min(6)).min(CAP_SECS))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAME: &str = "p1.example.com";

    fn ip(last: u8) -> Ipv4Addr {
        Ipv4Addr::new(95, 216, 0, last)
    }

    fn live(id: &str, kind: RecordKind, content: &str) -> LiveRecord {
        LiveRecord {
            id: id.to_string(),
            name: NAME.to_string(),
            kind,
            content: content.to_string(),
            proxied: false,
        }
    }

    fn managed(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn create(kind: RecordKind, content: &str) -> DnsOp {
        DnsOp::Create(NewRecord {
            name: NAME.to_string(),
            kind,
            content: content.to_string(),
        })
    }

    fn delete(id: &str) -> DnsOp {
        DnsOp::Delete(id.to_string())
    }

    #[test]
    fn creates_before_deletes_for_a_records() {
        let desired = RecordValue::A(vec![ip(2), ip(3)]);
        let records = [
            live("r1", RecordKind::A, "95.216.0.1"),
            live("r2", RecordKind::A, "95.216.0.2"),
        ];
        let ops = plan_changes(NAME, &desired, &records, &managed(&["r1", "r2"])).unwrap();
        assert_eq!(ops, vec![create(RecordKind::A, "95.216.0.3"), delete("r1")]);
    }

    #[test]
    fn cname_conflict_deletes_a_first() {
        let desired = RecordValue::Cname("p2.example.com".into());
        let records = [live("r1", RecordKind::A, "95.216.0.1")];
        let ops = plan_changes(NAME, &desired, &records, &managed(&["r1"])).unwrap();
        assert_eq!(
            ops,
            vec![delete("r1"), create(RecordKind::Cname, "p2.example.com")]
        );
    }

    #[test]
    fn a_over_cname_deletes_cname_first() {
        let desired = RecordValue::A(vec![ip(1)]);
        let records = [live("c1", RecordKind::Cname, "p2.example.com")];
        let ops = plan_changes(NAME, &desired, &records, &managed(&["c1"])).unwrap();
        assert_eq!(ops, vec![delete("c1"), create(RecordKind::A, "95.216.0.1")]);
    }

    #[test]
    fn empty_desired_is_error() {
        let records = [live("r1", RecordKind::A, "95.216.0.1")];
        let ids = managed(&["r1"]);
        assert_eq!(
            plan_changes(NAME, &RecordValue::A(vec![]), &records, &ids),
            Err(DnsPlanError::EmptyDesired)
        );
        assert_eq!(
            plan_changes(NAME, &RecordValue::Cname(String::new()), &records, &ids),
            Err(DnsPlanError::EmptyDesired)
        );
    }

    #[test]
    fn unmanaged_records_untouched() {
        let desired = RecordValue::A(vec![ip(2)]);
        let records = [
            live("u1", RecordKind::A, "95.216.0.9"),
            live("u2", RecordKind::Aaaa, "2001:db8::1"),
            live("u3", RecordKind::Other("TXT".into()), "hello"),
        ];
        let ops = plan_changes(NAME, &desired, &records, &managed(&[])).unwrap();
        assert_eq!(ops, vec![create(RecordKind::A, "95.216.0.2")]);
    }

    #[test]
    fn unmanaged_conflict_is_error_and_plans_nothing() {
        let desired = RecordValue::Cname("p2.example.com".into());
        let records = [
            live("r1", RecordKind::A, "95.216.0.1"),
            live("u1", RecordKind::A, "95.216.0.9"),
        ];
        assert_eq!(
            plan_changes(NAME, &desired, &records, &managed(&["r1"])),
            Err(DnsPlanError::UnmanagedConflict {
                record_id: "u1".into()
            })
        );
    }

    #[test]
    fn only_a_aaaa_cname_deleted() {
        let desired = RecordValue::A(vec![ip(1)]);
        let records = [
            live("m1", RecordKind::A, "95.216.0.7"),
            live("m2", RecordKind::Aaaa, "2001:db8::1"),
            live("m3", RecordKind::Other("TXT".into()), "hello"),
        ];
        let ops = plan_changes(NAME, &desired, &records, &managed(&["m1", "m2", "m3"])).unwrap();
        assert_eq!(
            ops,
            vec![
                create(RecordKind::A, "95.216.0.1"),
                delete("m1"),
                delete("m2")
            ]
        );
    }

    #[test]
    fn matching_record_stays_managed_or_not() {
        let desired = RecordValue::A(vec![ip(1), ip(2)]);
        let records = [
            live("u1", RecordKind::A, "95.216.0.1"),
            live("m1", RecordKind::A, "95.216.0.2"),
        ];
        let ops = plan_changes(NAME, &desired, &records, &managed(&["m1"])).unwrap();
        assert_eq!(ops, vec![]);
    }

    #[test]
    fn cname_retarget_deletes_old_cname_first() {
        let desired = RecordValue::Cname("p3.example.com".into());
        let records = [live("c1", RecordKind::Cname, "p2.example.com")];
        let ops = plan_changes(NAME, &desired, &records, &managed(&["c1"])).unwrap();
        assert_eq!(
            ops,
            vec![delete("c1"), create(RecordKind::Cname, "p3.example.com")]
        );
    }

    #[test]
    fn cname_match_ignores_case_and_trailing_dot() {
        let desired = RecordValue::Cname("P2.Example.com.".into());
        let records = [live("c1", RecordKind::Cname, "p2.example.com")];
        let ops = plan_changes(NAME, &desired, &records, &managed(&["c1"])).unwrap();
        assert_eq!(ops, vec![]);
    }

    #[test]
    fn plan_order_is_deterministic() {
        let desired = RecordValue::A(vec![ip(5), ip(3), ip(5), ip(4)]);
        let records = [
            live("z", RecordKind::A, "95.216.0.1"),
            live("a", RecordKind::A, "95.216.0.2"),
        ];
        let ids = managed(&["z", "a"]);
        let expected = vec![
            create(RecordKind::A, "95.216.0.3"),
            create(RecordKind::A, "95.216.0.4"),
            create(RecordKind::A, "95.216.0.5"),
            delete("a"),
            delete("z"),
        ];
        assert_eq!(
            plan_changes(NAME, &desired, &records, &ids).unwrap(),
            expected
        );
        let reversed = [records[1].clone(), records[0].clone()];
        assert_eq!(
            plan_changes(NAME, &desired, &reversed, &ids).unwrap(),
            expected
        );
    }

    #[test]
    fn backoff_5s_doubling_cap_5min() {
        let secs = |n| backoff(n).as_secs();
        assert_eq!(secs(0), 5);
        assert_eq!(secs(1), 10);
        assert_eq!(secs(2), 20);
        assert_eq!(secs(5), 160);
        assert_eq!(secs(6), 300);
        assert_eq!(secs(7), 300);
        assert_eq!(secs(31), 300);
        assert_eq!(secs(32), 300);
        assert_eq!(secs(1_000), 300);
        assert_eq!(secs(u32::MAX), 300);
    }
}
