use std::net::Ipv4Addr;

use mtpool_core::{EngineKind, NodeId, NodeRecord, NodeState, OwnerId, PoolState, Settings, Ts};

use super::*;

fn node(name: &str, state: NodeState) -> NodeRecord {
    let mut n = NodeRecord::new(
        NodeId::parse(name).unwrap(),
        OwnerId(1),
        Ipv4Addr::new(45, 33, 1, 5),
        443,
        EngineKind::Telemt,
        Ts(0),
        &Settings::default(),
    );
    n.state = state;
    n
}

fn text(nodes: Vec<NodeRecord>, extras: &Extras) -> String {
    let mut state = PoolState::new(Settings::default());
    for n in nodes {
        state.nodes.insert(n.id.clone(), n);
    }
    render_metrics(&Snapshot::build(&state), extras)
}

fn extras() -> Extras {
    Extras {
        bans_total: 0,
        store_healthy: true,
        deliveries: BTreeMap::new(),
    }
}

#[test]
fn counts_nodes_by_state_and_lists_every_state() {
    let out = text(
        vec![
            node("alpha-00001", NodeState::Healthy),
            node("alpha-00002", NodeState::Healthy),
            node("alpha-00003", NodeState::Quarantine),
            node("alpha-00004", NodeState::Banned),
        ],
        &extras(),
    );
    for line in [
        "mtpool_nodes{state=\"pending\"} 0",
        "mtpool_nodes{state=\"healthy\"} 2",
        "mtpool_nodes{state=\"quarantine\"} 1",
        "mtpool_nodes{state=\"banned\"} 1",
        "mtpool_nodes{state=\"dead\"} 0",
        "mtpool_nodes{state=\"pruned\"} 0",
    ] {
        assert!(out.lines().any(|l| l == line), "missing {line:?} in\n{out}");
    }
}

#[test]
fn the_other_figures() {
    let mut deliveries = BTreeMap::new();
    deliveries.insert("pending".to_owned(), 4);
    deliveries.insert("failed".to_owned(), 1);
    let out = text(
        Vec::new(),
        &Extras {
            bans_total: 7,
            store_healthy: false,
            deliveries,
        },
    );
    for line in [
        "mtpool_bans_total 7",
        "mtpool_store_healthy 0",
        "mtpool_webhook_deliveries{status=\"pending\"} 4",
        "mtpool_webhook_deliveries{status=\"delivered\"} 0",
        "mtpool_webhook_deliveries{status=\"failed\"} 1",
    ] {
        assert!(out.lines().any(|l| l == line), "missing {line:?} in\n{out}");
    }
    let build = format!(
        "mtpool_build_info{{version=\"{}\"}} 1",
        env!("CARGO_PKG_VERSION")
    );
    assert!(out.lines().any(|l| l == build), "{out}");
    assert!(text(Vec::new(), &extras()).contains("mtpool_store_healthy 1\n"));
}

#[test]
fn masters_by_domain_with_escaped_labels() {
    let mut state = PoolState::new(Settings {
        domains: vec!["a.example.com".into(), "b.example.com".into()],
        ..Settings::default()
    });
    let n = node("alpha-00001", NodeState::Healthy);
    state.nodes.insert(n.id.clone(), n);
    let out = render_metrics(&Snapshot::build(&state), &extras());
    // Every configured domain has a line, also one without a master.
    assert!(
        out.contains("mtpool_masters{domain=\"a.example.com\"} 0\n"),
        "{out}"
    );
    assert!(
        out.contains("mtpool_masters{domain=\"b.example.com\"} 0\n"),
        "{out}"
    );
    assert_eq!(label_value("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
}

#[test]
fn text_format_is_well_formed() {
    let out = text(vec![node("alpha-00001", NodeState::Healthy)], &extras());
    assert!(out.ends_with('\n'));
    // Each family has its HELP and TYPE once, before its samples.
    for (name, kind) in [
        ("mtpool_nodes", "gauge"),
        ("mtpool_masters", "gauge"),
        ("mtpool_bans_total", "counter"),
        ("mtpool_store_healthy", "gauge"),
        ("mtpool_webhook_deliveries", "gauge"),
        ("mtpool_build_info", "gauge"),
    ] {
        assert_eq!(
            out.matches(&format!("# TYPE {name} {kind}\n")).count(),
            1,
            "{name}"
        );
        assert_eq!(out.matches(&format!("# HELP {name} ")).count(), 1, "{name}");
    }
}
