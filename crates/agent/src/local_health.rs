//! The node's own look at its proxy: the engine says it is healthy and its port accepts a local
//! connection. Three bad checks in a row make the node silent, two good ones bring it back.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use async_trait::async_trait;
use tokio::time::Instant;

use crate::engine::{Engine, EngineHealth};

/// Bad checks in a row before the node goes silent.
pub const SILENT_AFTER: u32 = 3;
/// Good checks in a row before a silent node is back.
pub const BACK_AFTER: u32 = 2;

/// How long one connect may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Whether something accepts connections on a local port.
#[async_trait]
pub trait PortCheck: Send + Sync {
    /// `port` is tried on the loopback addresses and, if given, on `also` (the node's own public
    /// address, for a proxy bound to it alone).
    async fn listening(&self, port: u16, also: Option<Ipv4Addr>) -> bool;
}

/// A real TCP connect, a few seconds at most per address.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalTcp;

#[async_trait]
impl PortCheck for LocalTcp {
    async fn listening(&self, port: u16, also: Option<Ipv4Addr>) -> bool {
        let mut addresses = vec![
            SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
        ];
        addresses.extend(also.map(|ip| SocketAddr::from((ip, port))));
        for address in addresses {
            let connect = tokio::net::TcpStream::connect(address);
            if matches!(
                tokio::time::timeout(CONNECT_TIMEOUT, connect).await,
                Ok(Ok(_))
            ) {
                return true;
            }
        }
        false
    }
}

/// The outcome of one check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// The engine says it is healthy and the port accepts a connection.
    pub ok: bool,
    pub listening: bool,
    pub health: EngineHealth,
}

/// One check: the engine's own verdict and a connect to its port. No port is a bad check.
pub async fn check_once(
    engine: &dyn Engine,
    ports: &dyn PortCheck,
    port: Option<u16>,
    also: Option<Ipv4Addr>,
) -> Check {
    let health = engine.health().await;
    let listening = match port {
        Some(port) => ports.listening(port, also).await,
        None => false,
    };
    Check {
        ok: health.ok && listening,
        listening,
        health,
    }
}

/// The run of good and bad checks.
#[derive(Debug, Clone, Default)]
pub struct LocalHealth {
    bad: u32,
    good: u32,
    silent: bool,
    bad_since: Option<Instant>,
}

impl LocalHealth {
    pub fn new() -> LocalHealth {
        LocalHealth::default()
    }

    /// Takes one check made at `now`.
    pub fn observe(&mut self, now: Instant, ok: bool) {
        if ok {
            self.good = self.good.saturating_add(1);
            self.bad = 0;
            // A silent node is not back before two good checks; a bad run that never reached
            // three is over at the first good check.
            if !self.silent || self.good >= BACK_AFTER {
                self.silent = false;
                self.bad_since = None;
            }
        } else {
            self.bad = self.bad.saturating_add(1);
            self.good = 0;
            self.bad_since.get_or_insert(now);
            if self.bad >= SILENT_AFTER {
                self.silent = true;
            }
        }
    }

    pub fn is_silent(&self) -> bool {
        self.silent
    }

    /// When the bad run that has not been cleared yet began.
    pub fn bad_since(&self) -> Option<Instant> {
        self.bad_since
    }
}
