//! The rollout of a new agent release to the nodes.
//!
//! The rules are in `mtpool_core::rollout` and have no I/O. This module holds the current
//! rollout, feeds it with what agents report and what the state owner knows, saves it, and
//! starts a rollout when a signed release with a newer agent appears in the releases directory.
//! It never calls the network and the state owner never waits for it: the worker only reads
//! the state owner through `inspect`.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use mtpool_core::rollout::{Reason, Rollout, Status};
use mtpool_core::{NodeId, NodeRecord, RolloutSettings, Ts};
use mtpool_selfupdate::manifest::{
    AGENT_COMPONENT, MAX_MANIFEST_BYTES, Version, verify_manifest_with,
};
use mtpool_selfupdate::{MANIFEST_NAME, SIGNATURE_NAME, read_release_file};

use crate::actor::{ActorHandle, Clock};
use crate::store::{AdminOp, Store, StoreError};

/// How often the worker looks at the fleet.
const TICK: Duration = Duration::from_secs(1);
/// How often the releases directory is looked at.
const SCAN_EVERY: Duration = Duration::from_secs(10);

#[derive(Default)]
struct Inner {
    current: Option<Rollout>,
    /// The newest version a rollout was ever started for.
    newest: Option<Version>,
    dirty: bool,
}

/// The rollout in progress or the last one, and its saved copy.
#[derive(Clone)]
pub struct RolloutService {
    inner: Arc<Mutex<Inner>>,
    store: Store,
    clock: Clock,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Every saved rollout, newest first.
fn load_all(store: &Store) -> Result<Vec<Rollout>, StoreError> {
    let conn = store.read();
    let mut stmt = conn.prepare("SELECT state FROM rollouts ORDER BY id DESC")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut all = Vec::new();
    for row in rows {
        match serde_json::from_str::<Rollout>(&row?) {
            Ok(r) => all.push(r),
            Err(e) => tracing::error!(error = %e, "a saved rollout cannot be read, skipped"),
        }
    }
    Ok(all)
}

impl RolloutService {
    /// Picks up where the registrar was before it stopped.
    pub fn load(store: Store, clock: Clock) -> Result<RolloutService, StoreError> {
        let all = load_all(&store)?;
        let newest = all.iter().filter_map(|r| Version::parse(&r.version)).max();
        let current = all.into_iter().next();
        Ok(RolloutService {
            inner: Arc::new(Mutex::new(Inner {
                current,
                newest,
                dirty: false,
            })),
            store,
            clock,
        })
    }

    /// A service with nothing in it.
    pub fn empty(store: Store, clock: Clock) -> RolloutService {
        RolloutService {
            inner: Arc::new(Mutex::new(Inner::default())),
            store,
            clock,
        }
    }

    /// A node polled with the version it runs. `true` when it is told to update.
    pub fn poll(&self, node: &NodeId, version: &str, now: Ts) -> bool {
        let mut inner = lock(&self.inner);
        let Some(r) = inner.current.as_mut().filter(|r| r.is_open()) else {
            return false;
        };
        let before = r.clone();
        r.observe_version(node, version, now);
        let offer = r.offer_for(node);
        if *r != before {
            inner.dirty = true;
        }
        offer
    }

    /// The version of the open rollout.
    pub fn offered_version(&self) -> Option<String> {
        lock(&self.inner)
            .current
            .as_ref()
            .filter(|r| r.is_open())
            .map(|r| r.version.clone())
    }

    /// What a node said about its update.
    pub fn status(&self, node: &NodeId, version: &str, status: &Status, now: Ts) {
        let mut inner = lock(&self.inner);
        // A report about another version is a late one and says nothing about this rollout.
        if let Some(r) = inner
            .current
            .as_mut()
            .filter(|r| r.is_open() && r.version == version)
        {
            r.on_status(node, status, now);
            inner.dirty = true;
        }
    }

    /// The rollout with `id`, the current one included.
    pub fn get(&self, id: i64) -> Result<Option<Rollout>, StoreError> {
        Ok(self.list()?.into_iter().find(|r| r.id == id))
    }

    /// Every rollout, newest first. The current one is taken from memory, which is never
    /// behind the saved copy.
    pub fn list(&self) -> Result<Vec<Rollout>, StoreError> {
        let mut all = load_all(&self.store)?;
        if let Some(current) = lock(&self.inner).current.clone() {
            all.retain(|r| r.id != current.id);
            all.insert(0, current);
        }
        Ok(all)
    }

    /// "Continue". `None` when there is no such rollout; `Some(false)` when it is not paused.
    pub fn resume(&self, id: i64, nodes: &[NodeRecord], now: Ts) -> Option<bool> {
        let mut inner = lock(&self.inner);
        let r = inner.current.as_mut().filter(|r| r.id == id)?;
        let done = r.resume(nodes, now);
        inner.dirty |= done;
        Some(done)
    }

    /// "Cancel". `None` when there is no such rollout; `Some(false)` when it is over already.
    pub fn cancel(&self, id: i64, now: Ts) -> Option<bool> {
        let mut inner = lock(&self.inner);
        let r = inner.current.as_mut().filter(|r| r.id == id)?;
        let done = r.cancel(now);
        inner.dirty |= done;
        Some(done)
    }

    /// Starts a rollout of `version` when it is newer than every one before. An open rollout
    /// of an older version is cancelled. `true` when one was started.
    pub fn offer_release(&self, version: &str, now: Ts, timings: RolloutSettings) -> bool {
        let Some(parsed) = Version::parse(version) else {
            return false;
        };
        let mut inner = lock(&self.inner);
        if inner.newest.as_ref().is_some_and(|n| *n >= parsed) {
            return false;
        }
        if let Some(old) = inner.current.as_mut()
            && old.cancel(now)
        {
            old.note = Some(Reason {
                code: "superseded".to_owned(),
                text: format!("версия {version} вышла раньше, чем закончилась эта раскатка"),
            });
        }
        let old = inner.current.take();
        let id = match &old {
            Some(r) => r.id,
            None => 0,
        };
        // Ids only grow; the saved rows may hold a later one than the one in memory.
        let saved = load_all(&self.store)
            .ok()
            .and_then(|all| all.iter().map(|r| r.id).max())
            .unwrap_or(0);
        let id = id.max(saved) + 1;
        if let Some(old) = old {
            self.save(&old);
        }
        let fresh = Rollout::new(id, version, now, timings);
        self.save(&fresh);
        inner.current = Some(fresh);
        inner.newest = Some(parsed);
        inner.dirty = false;
        true
    }

    /// Moves the rollout on and saves it when it changed.
    pub fn tick(&self, nodes: &[NodeRecord], masters: &BTreeSet<NodeId>, now: Ts) {
        let snapshot = {
            let mut inner = lock(&self.inner);
            if let Some(r) = inner.current.as_mut().filter(|r| r.is_open()) {
                let before = r.clone();
                r.advance(nodes, masters, now);
                if *r != before {
                    inner.dirty = true;
                }
            }
            if !inner.dirty {
                return;
            }
            inner.dirty = false;
            inner.current.clone()
        };
        if let Some(r) = snapshot {
            self.save(&r);
        }
    }

    /// Writes the current rollout now (after an admin action).
    pub fn flush(&self) {
        let snapshot = {
            let mut inner = lock(&self.inner);
            inner.dirty = false;
            inner.current.clone()
        };
        if let Some(r) = snapshot {
            self.save(&r);
        }
    }

    fn save(&self, r: &Rollout) {
        let state = match serde_json::to_string(r) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "cannot serialize the rollout");
                return;
            }
        };
        let op = AdminOp::SaveRollout {
            id: r.id,
            version: r.version.clone(),
            state,
            created_at: r.created_at,
            updated_at: (self.clock)(),
        };
        if let Err(e) = self.store.admin(op) {
            tracing::error!(error = %e, rollout = r.id, "cannot save the rollout");
        }
    }
}

/// The version of the agent in a signed release in `dir`, if there is one.
pub fn release_agent_version(dir: &Path, key: &str) -> Option<String> {
    let manifest = read_release_file(dir, MANIFEST_NAME, MAX_MANIFEST_BYTES as u64).ok()?;
    let signature = read_release_file(dir, SIGNATURE_NAME, 16 * 1024).ok()?;
    let signature = String::from_utf8(signature).ok()?;
    let manifest = verify_manifest_with(key, &manifest, &signature).ok()?;
    manifest
        .artifacts
        .iter()
        .any(|a| a.component == AGENT_COMPONENT)
        .then(|| manifest.version.to_string())
}

/// The worker: looks for a new release and drives the rollout.
pub async fn run(
    service: RolloutService,
    actor: ActorHandle,
    clock: Clock,
    releases_dir: Arc<std::path::PathBuf>,
    key: &'static str,
) {
    let mut last_scan = None::<tokio::time::Instant>;
    loop {
        tokio::time::sleep(TICK).await;
        if actor.is_stopped() {
            return;
        }
        let now = clock();
        let Ok((nodes, masters, timings)) = actor
            .inspect(|state| {
                let masters: BTreeSet<NodeId> = state
                    .assignments
                    .0
                    .values()
                    .flatten()
                    .map(|slot| slot.node.clone())
                    .collect();
                (
                    state.nodes.values().cloned().collect::<Vec<_>>(),
                    masters,
                    state.settings.rollout,
                )
            })
            .await
        else {
            return;
        };
        if !key.trim().is_empty() && last_scan.is_none_or(|t| t.elapsed() >= SCAN_EVERY) {
            last_scan = Some(tokio::time::Instant::now());
            let dir = Arc::clone(&releases_dir);
            let found = tokio::task::spawn_blocking(move || release_agent_version(&dir, key))
                .await
                .ok()
                .flatten();
            if let Some(version) = found {
                let svc = service.clone();
                let started =
                    tokio::task::spawn_blocking(move || svc.offer_release(&version, now, timings))
                        .await
                        .unwrap_or(false);
                if started {
                    tracing::info!("a rollout of a new agent release starts");
                }
            }
        }
        let svc = service.clone();
        let _ = tokio::task::spawn_blocking(move || svc.tick(&nodes, &masters, now)).await;
    }
}

#[cfg(test)]
mod tests;
