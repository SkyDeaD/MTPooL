//! The TCP prober: every few seconds it connects to each live node from outside and reports
//! whether the port answered, as `Command::TcpProbe`.
//!
//! The state owner never waits for it. A round reads the targets through
//! `ActorHandle::inspect` (one quick read), probes them without touching the owner and sends
//! one command per result. The generation travels with the target, so the answer for a node
//! that was replaced meanwhile is dropped by `core`.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use mtpool_core::{Command, Generation, NodeId, NodeState, PoolState};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::MissedTickBehavior;

use crate::actor::ActorHandle;

#[cfg(test)]
mod tests;

/// Connections in flight at once.
pub const MAX_CONCURRENT: usize = 64;

/// One node to probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub node: NodeId,
    pub generation: Generation,
    pub addr: SocketAddr,
}

pub type ConnectFuture<'a> = Pin<Box<dyn Future<Output = bool> + Send + 'a>>;

/// Opens a TCP connection and says whether it was accepted. The real one in the service, a
/// scripted one in tests.
pub trait Connector: Send + Sync + 'static {
    fn connect(&self, addr: SocketAddr) -> ConnectFuture<'_>;
}

/// `TcpStream::connect`.
#[derive(Debug, Default, Clone, Copy)]
pub struct TcpConnector;

impl Connector for TcpConnector {
    fn connect(&self, addr: SocketAddr) -> ConnectFuture<'_> {
        Box::pin(async move { tokio::net::TcpStream::connect(addr).await.is_ok() })
    }
}

/// Probes every target, at most `limit` at a time, each under `timeout`. A connection that is
/// not accepted in time counts as closed.
pub async fn probe_all(
    connector: &Arc<dyn Connector>,
    targets: Vec<Target>,
    timeout: Duration,
    limit: usize,
) -> Vec<(NodeId, Generation, bool)> {
    let permits = Arc::new(Semaphore::new(limit.max(1)));
    let mut probes = JoinSet::new();
    for target in targets {
        // The permit is taken before the task starts: no more than `limit` exist at a time.
        let Ok(permit) = Arc::clone(&permits).acquire_owned().await else {
            break;
        };
        let connector = Arc::clone(connector);
        probes.spawn(async move {
            let open = tokio::time::timeout(timeout, connector.connect(target.addr))
                .await
                .unwrap_or(false);
            drop(permit);
            (target.node, target.generation, open)
        });
    }
    let mut results = Vec::with_capacity(probes.len());
    while let Some(done) = probes.join_next().await {
        match done {
            Ok(result) => results.push(result),
            Err(e) => tracing::error!(error = %e, "a TCP probe task failed"),
        }
    }
    results
}

/// What a round needs from the live state.
struct Round {
    targets: Vec<Target>,
    interval: Duration,
    timeout: Duration,
}

fn read_round(state: &PoolState) -> Round {
    let targets = state
        .nodes
        .values()
        .filter(|n| {
            matches!(
                n.state,
                NodeState::Pending | NodeState::Healthy | NodeState::Quarantine
            )
        })
        .map(|n| Target {
            node: n.id.clone(),
            generation: n.generation,
            addr: SocketAddr::from((n.ip, n.port)),
        })
        .collect();
    Round {
        targets,
        interval: Duration::from_secs(u64::from(state.settings.tcp_probe_interval_sec.max(1))),
        timeout: Duration::from_secs(u64::from(state.settings.tcp_probe_timeout_sec.max(1))),
    }
}

pub struct Prober {
    connector: Arc<dyn Connector>,
    interval: Option<Duration>,
}

impl Default for Prober {
    fn default() -> Prober {
        Prober::new()
    }
}

impl Prober {
    pub fn new() -> Prober {
        Prober::with_connector(Arc::new(TcpConnector))
    }

    pub fn with_connector(connector: Arc<dyn Connector>) -> Prober {
        Prober {
            connector,
            interval: None,
        }
    }

    /// Probes on this interval instead of the one in the settings.
    pub fn with_interval(mut self, interval: Duration) -> Prober {
        self.interval = Some(interval);
        self
    }

    /// Runs until the state owner is gone. A round that is still going when the next one is
    /// due is not overlapped: the missed tick is skipped.
    pub async fn run(self, actor: ActorHandle) {
        let mut ticker: Option<(Duration, tokio::time::Interval)> = None;
        loop {
            if let Some((_, ticker)) = ticker.as_mut() {
                tokio::select! {
                    () = actor.stopped() => return,
                    _ = ticker.tick() => {}
                }
            }
            // Read after the wait, so the nodes and the settings are current.
            let Ok(round) = actor.inspect(read_round).await else {
                return;
            };
            let interval = self.interval.unwrap_or(round.interval);
            // The settings may change: a new interval starts a new ticker (its first tick is
            // the one that is due now).
            if ticker
                .as_ref()
                .is_none_or(|(current, _)| *current != interval)
            {
                let mut fresh = tokio::time::interval(interval);
                fresh.set_missed_tick_behavior(MissedTickBehavior::Skip);
                fresh.tick().await;
                ticker = Some((interval, fresh));
            }
            let results = probe_all(
                &self.connector,
                round.targets,
                round.timeout,
                MAX_CONCURRENT,
            )
            .await;
            for (node, generation, open) in results {
                let probe = Command::TcpProbe {
                    node,
                    generation,
                    open,
                };
                if actor.send(probe).await.is_err() {
                    return;
                }
            }
        }
    }
}
