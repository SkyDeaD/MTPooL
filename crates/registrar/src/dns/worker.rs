//! The DNS worker: the driver that answers `Effect::PushDns`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use mtpool_core::{Command, DnsOp, Effect, Generation, RecordValue, plan_changes};
use tokio::sync::Mutex as AsyncMutex;

use super::DnsProvider;
use crate::actor::{ActorHandle, DriverFuture, EffectDriver};

#[cfg(test)]
mod tests;

/// Pushes the desired records of a domain to the provider.
///
/// One task per push, so the state owner never waits for the network. The worker raises no
/// event: it reports `Command::DnsDone` and `core` decides what a failure means (the event and
/// the backoff).
#[derive(Clone)]
pub struct DnsWorker {
    provider: Arc<dyn DnsProvider>,
    /// One push of a domain at a time, so two pushes never plan from the same stale list.
    locks: Arc<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>>,
}

impl DnsWorker {
    pub fn new(provider: Arc<dyn DnsProvider>) -> DnsWorker {
        DnsWorker {
            provider,
            locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn lock_of(&self, domain: &str) -> Arc<AsyncMutex<()>> {
        let mut locks = self.locks.lock().expect("domain locks");
        Arc::clone(locks.entry(domain.to_owned()).or_default())
    }

    /// Looks up the zone, reads the records, plans and applies the changes (every create before
    /// any delete). The first error ends the sequence; the result names it by a short reason.
    pub async fn push(
        &self,
        domain: String,
        generation: Generation,
        desired: RecordValue,
        ttl: u32,
        proxied: bool,
    ) -> Command {
        let lock = self.lock_of(&domain);
        let _one_at_a_time = lock.lock().await;
        let result = self.apply(&domain, &desired, ttl, proxied).await;
        Command::DnsDone {
            domain,
            generation,
            result,
        }
    }

    async fn apply(
        &self,
        domain: &str,
        desired: &RecordValue,
        ttl: u32,
        proxied: bool,
    ) -> Result<(), String> {
        let reason = |e: super::DnsError| e.reason();
        let zone = self
            .provider
            .zone_id(domain)
            .await
            .map_err(reason)?
            .ok_or_else(|| "zone_not_found".to_owned())?;
        let records = self.provider.list(&zone, domain).await.map_err(reason)?;
        let live: Vec<_> = records.iter().map(|r| r.live.clone()).collect();
        let managed: HashSet<String> = records
            .iter()
            .filter(|r| r.managed)
            .map(|r| r.live.id.clone())
            .collect();
        let ops =
            plan_changes(domain, desired, &live, &managed).map_err(|e| format!("plan:{e}"))?;
        for op in ops {
            match op {
                DnsOp::Create(record) => {
                    self.provider
                        .create(&zone, record, ttl, proxied)
                        .await
                        .map_err(reason)?;
                }
                DnsOp::Delete(id) => {
                    self.provider.delete(&zone, &id).await.map_err(reason)?;
                }
            }
        }
        Ok(())
    }
}

impl EffectDriver for DnsWorker {
    fn handles(&self, effect: &Effect) -> bool {
        matches!(effect, Effect::PushDns { .. })
    }

    fn run(&self, effect: Effect, results: ActorHandle) -> DriverFuture {
        let worker = self.clone();
        Box::pin(async move {
            let Effect::PushDns {
                domain,
                generation,
                desired,
            } = effect
            else {
                return;
            };
            let Ok((ttl, proxied)) = results
                .inspect(|s| (s.settings.dns_ttl, s.settings.dns_proxied))
                .await
            else {
                return;
            };
            let done = worker.push(domain, generation, desired, ttl, proxied).await;
            // A stopped owner has nobody to tell.
            let _ = results.send(done).await;
        })
    }
}
