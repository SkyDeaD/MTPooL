//! Base identifiers, enums and IP helpers.

use std::fmt;
use std::net::Ipv4Addr;

/// Unix time in milliseconds. The core never reads the clock: callers pass it in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ts(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OwnerId(pub i64);

/// Version of a domain's desired DNS state; checked before applying a worker result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Generation(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EngineKind {
    Telemt,
    MtProxyL,
    Meko,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeState {
    Pending,
    Healthy,
    Quarantine,
    Banned,
    Dead,
    Pruned,
}

/// Node name: `<name>-<5 chars>`.
///
/// The name is 1 to 32 ASCII letters, digits and `._-`, starting and ending with a
/// letter or digit. The suffix is five lowercase letters or digits.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdError {
    InvalidFormat,
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdError::InvalidFormat => f.write_str("node id must look like <name>-<5 chars>"),
        }
    }
}

impl std::error::Error for IdError {}

impl NodeId {
    pub fn parse(s: &str) -> Result<NodeId, IdError> {
        let (name, suffix) = s.rsplit_once('-').ok_or(IdError::InvalidFormat)?;
        let suffix_ok = suffix.len() == 5
            && suffix
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
        let name = name.as_bytes();
        let edge_ok = |b: &u8| b.is_ascii_alphanumeric();
        let name_ok = (1..=32).contains(&name.len())
            && name.first().is_some_and(edge_ok)
            && name.last().is_some_and(edge_ok)
            && name
                .iter()
                .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if suffix_ok && name_ok {
            Ok(NodeId(s.to_owned()))
        } else {
            Err(IdError::InvalidFormat)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// True for addresses a client on the internet could reach: excludes private,
/// loopback, link-local, shared (CGNAT), documentation, benchmarking, multicast,
/// reserved and unspecified ranges.
pub fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    let special = a == 0
        || a == 10
        || (a == 100 && (64..=127).contains(&b))
        || a == 127
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 0 && (c == 0 || c == 2))
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 224;
    !special
}

/// `95.216.0.1` becomes `95.x.x.1`, for logs and public pages.
pub fn mask_ip(ip: Ipv4Addr) -> String {
    let [a, _, _, d] = ip.octets();
    format!("{a}.x.x.{d}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    #[test]
    fn node_id_accepts_valid_names() {
        assert!(NodeId::parse("LeroVPN-123-a1b2c").is_ok());
        assert!(NodeId::parse("a-a1b2c").is_ok());
        assert!(NodeId::parse("a.b_c-d-00000").is_ok());
        assert!(NodeId::parse(&format!("{}-a1b2c", "x".repeat(32))).is_ok());
        assert_eq!(NodeId::parse("host-a1b2c").unwrap().as_str(), "host-a1b2c");
        assert_eq!(
            NodeId::parse("host-a1b2c").unwrap().to_string(),
            "host-a1b2c"
        );
    }

    #[test]
    fn node_id_rejects_invalid_names() {
        assert!(NodeId::parse("-a1b2c").is_err());
        assert!(NodeId::parse(&format!("{}-a1b2c", "x".repeat(33))).is_err());
        assert!(NodeId::parse("").is_err());
        assert!(NodeId::parse("host").is_err());
        assert!(NodeId::parse("host-a1b2").is_err(), "suffix too short");
        assert!(NodeId::parse("host-a1b2c3").is_err(), "suffix too long");
        assert!(NodeId::parse("host-A1B2C").is_err(), "suffix is lowercase");
        assert!(
            NodeId::parse("host--a1b2c").is_err(),
            "name ends with a dash"
        );
        assert!(
            NodeId::parse(".host-a1b2c").is_err(),
            "name starts with a dot"
        );
        assert!(NodeId::parse("ho st-a1b2c").is_err());
        assert!(NodeId::parse("хост-a1b2c").is_err(), "non-ASCII name");
        assert!(NodeId::parse("host-a1b2c\n").is_err());
    }

    #[test]
    fn ts_orders_by_millis() {
        assert!(Ts(1) < Ts(2));
        assert_eq!(Ts(5), Ts(5));
    }

    #[test]
    fn public_ipv4_rejects_special_ranges() {
        for s in [
            "0.0.0.0",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.0.1",
            "192.0.2.1",
            "192.168.1.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
        ] {
            assert!(!is_public_ipv4(ip(s)), "{s} must not be public");
        }
    }

    #[test]
    fn public_ipv4_accepts_ordinary_addresses() {
        for s in [
            "1.1.1.1",
            "8.8.8.8",
            "95.216.0.1",
            "172.15.0.1",
            "172.32.0.1",
            "100.63.0.1",
            "100.128.0.1",
        ] {
            assert!(is_public_ipv4(ip(s)), "{s} must be public");
        }
    }

    #[test]
    fn mask_ip_hides_middle_octets() {
        assert_eq!(mask_ip(ip("95.216.0.1")), "95.x.x.1");
        assert_eq!(mask_ip(ip("1.2.3.4")), "1.x.x.4");
    }

    #[test]
    fn enums_are_distinct() {
        assert_ne!(EngineKind::Telemt, EngineKind::Meko);
        assert_ne!(NodeState::Pending, NodeState::Healthy);
        assert_eq!(Generation(3), Generation(3));
        assert_eq!(OwnerId(7), OwnerId(7));
    }
}
