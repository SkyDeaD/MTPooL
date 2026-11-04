//! What a simulated node and a viewer look like to the registrar.

use std::net::Ipv4Addr;

use mtpool_protocol::public::NodeState;
use serde::Serialize;
use serde_json::{Value, json};

/// The address of node `index`, as the registrar sees it through the last `X-Forwarded-For`
/// entry of a loopback peer: `127.x.y.z`, never `127.0.0.0`.
pub fn node_ip(index: u32) -> Ipv4Addr {
    let n = index + 1;
    Ipv4Addr::new(127, (n >> 16) as u8, (n >> 8) as u8, n as u8)
}

/// The address of viewer `index`: the benchmarking range `198.18.0.0/15`.
pub fn viewer_ip(index: u32) -> Ipv4Addr {
    Ipv4Addr::new(198, 18, (index >> 8) as u8, index as u8)
}

/// The body of `POST /v1/measurements` as the agent sends it (`mtpool_agent::gp::measurement_body`):
/// an HTTPS `HEAD` of the proxy port from 20 Russian eyeball networks. The registrar checks every
/// field of this against the node, so a drift shows up as nodes that never become healthy.
pub fn measurement_body(ip: Ipv4Addr, port: u16, sni: &str) -> Value {
    json!({
        "type": "http",
        "target": ip.to_string(),
        "locations": [{ "country": "RU", "tags": ["eyeball-network"], "limit": 20 }],
        "measurementOptions": {
            "port": port,
            "protocol": "HTTPS",
            "request": { "method": "HEAD", "host": sni, "path": "/" }
        }
    })
}

/// The targets of section 13 of the specification.
pub const HEARTBEAT_P99_MS: f64 = 20.0;
pub const NODES_P99_MS: f64 = 20.0;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Verdict {
    pub heartbeat_p99_under_20ms: bool,
    pub nodes_p99_under_20ms: bool,
    pub no_errors: bool,
    pub all: bool,
}

/// Compares the measured figures with the targets. A missing figure is a miss.
pub fn verdict(heartbeat_p99: Option<f64>, nodes_p99: Option<f64>, errors: u64) -> Verdict {
    let heartbeat = heartbeat_p99.is_some_and(|p| p < HEARTBEAT_P99_MS);
    let nodes = nodes_p99.is_some_and(|p| p < NODES_P99_MS);
    Verdict {
        heartbeat_p99_under_20ms: heartbeat,
        nodes_p99_under_20ms: nodes,
        no_errors: errors == 0,
        all: heartbeat && nodes && errors == 0,
    }
}

/// How many nodes are in each state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct StateCounts {
    pub pending: u32,
    pub healthy: u32,
    pub quarantine: u32,
    pub banned: u32,
    pub dead: u32,
    pub pruned: u32,
}

impl StateCounts {
    pub fn add(&mut self, state: NodeState) {
        match state {
            NodeState::Pending => self.pending += 1,
            NodeState::Healthy => self.healthy += 1,
            NodeState::Quarantine => self.quarantine += 1,
            NodeState::Banned => self.banned += 1,
            NodeState::Dead => self.dead += 1,
            NodeState::Pruned => self.pruned += 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_addresses_are_distinct_loopback_and_never_zero() {
        assert_eq!(node_ip(0), Ipv4Addr::new(127, 0, 0, 1));
        assert_eq!(node_ip(999), Ipv4Addr::new(127, 0, 3, 232));
        let all: std::collections::HashSet<_> = (0..70_000).map(node_ip).collect();
        assert_eq!(all.len(), 70_000);
        assert!(all.iter().all(|ip| ip.is_loopback()));
    }

    #[test]
    fn viewer_addresses_are_distinct_and_not_loopback() {
        let all: std::collections::HashSet<_> = (0..2000).map(viewer_ip).collect();
        assert_eq!(all.len(), 2000);
        assert!(all.iter().all(|ip| !ip.is_loopback() && !ip.is_private()));
    }

    #[test]
    fn measurement_body_has_the_shape_the_registrar_checks() {
        let body = measurement_body(Ipv4Addr::new(127, 0, 0, 5), 443, "www.example.com");
        assert_eq!(body["type"], "http");
        assert_eq!(body["target"], "127.0.0.5");
        assert_eq!(body["locations"][0]["country"], "RU");
        assert_eq!(body["locations"][0]["tags"][0], "eyeball-network");
        assert_eq!(body["measurementOptions"]["port"], 443);
        assert_eq!(body["measurementOptions"]["protocol"], "HTTPS");
        assert_eq!(
            body["measurementOptions"]["request"]["host"],
            "www.example.com"
        );
    }

    #[test]
    fn verdict_needs_every_target() {
        assert!(verdict(Some(5.0), Some(19.9), 0).all);
        assert!(!verdict(Some(20.0), Some(5.0), 0).all);
        assert!(!verdict(Some(5.0), Some(25.0), 0).all);
        assert!(!verdict(Some(5.0), Some(5.0), 1).all);
        assert!(!verdict(None, Some(5.0), 0).all);
    }

    #[test]
    fn state_counts_add_up() {
        let mut c = StateCounts::default();
        c.add(NodeState::Healthy);
        c.add(NodeState::Healthy);
        c.add(NodeState::Quarantine);
        assert_eq!((c.healthy, c.quarantine, c.pending), (2, 1, 0));
    }
}
