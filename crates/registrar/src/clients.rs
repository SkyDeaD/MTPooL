//! The client sampler: every 30 seconds it reads the client numbers off the state owner and
//! queues them for the store, for the history chart.
//!
//! The state owner is asked once through `ActorHandle::inspect` (a quick read); the write is
//! a non-blocking send to the store writer. Nothing here waits for the disk.

use std::time::Duration;

use mtpool_core::{Command, Ts, client_sample, node_samples};
use tokio::time::MissedTickBehavior;

use crate::actor::{ActorHandle, ActorStopped, Clock};
use crate::store::{HistoryOp, Store, availability};

/// How often a sample is taken, and the grid its times are aligned to.
pub const INTERVAL: Duration = Duration::from_secs(30);

#[cfg(test)]
mod tests;

/// The time of the sample: `now` floored to the grid, so the rows of two runs of the service
/// fall on the same moments and a restart does not leave a gap of its own making.
pub fn aligned(now: Ts) -> Ts {
    let step = i64::try_from(INTERVAL.as_millis()).unwrap_or(30_000);
    Ts(now.0.div_euclid(step) * step)
}

/// Takes one sample and queues it.
pub async fn sample_once(
    actor: &ActorHandle,
    store: &Store,
    clock: &Clock,
) -> Result<(), ActorStopped> {
    let now = clock();
    let (sample, nodes) = actor
        .inspect(move |state| (client_sample(state, now), node_samples(state)))
        .await?;
    let at = aligned(sample.at);
    store.send(HistoryOp::ClientSamples {
        at,
        rows: sample.rows(),
    });
    if !nodes.is_empty() {
        store.send(HistoryOp::NodeSamples { at, rows: nodes });
    }
    Ok(())
}

/// How often the 30-day availability is counted again.
pub const AVAILABILITY_INTERVAL: Duration = Duration::from_secs(300);

/// Counts the 30-day availability from the stored samples and hands it to the state owner.
/// The read runs off the async threads. A failed read leaves the old numbers in place.
pub async fn refresh_availability(
    actor: &ActorHandle,
    store: &Store,
    clock: &Clock,
) -> Result<(), ActorStopped> {
    let now = clock();
    let reader = store.clone();
    let counted = tokio::task::spawn_blocking(move || availability(&reader.read(), now)).await;
    match counted {
        Ok(Ok(list)) => actor.send(Command::AvailabilityRefreshed(list)).await,
        Ok(Err(error)) => {
            tracing::warn!(%error, "cannot count the availability");
            Ok(())
        }
        Err(error) => {
            tracing::warn!(%error, "the availability count did not finish");
            Ok(())
        }
    }
}

/// Refreshes the availability at the start and then every `AVAILABILITY_INTERVAL`, until the
/// state owner is gone.
pub async fn run_availability(actor: ActorHandle, store: Store, clock: Clock) {
    let mut ticker = tokio::time::interval(AVAILABILITY_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            () = actor.stopped() => return,
            _ = ticker.tick() => {}
        }
        if refresh_availability(&actor, &store, &clock).await.is_err() {
            return;
        }
    }
}

/// How often old rows are deleted.
pub const RETENTION_INTERVAL: Duration = Duration::from_secs(3600);

/// Deletes the rows past their retention period. `Store::retain` blocks the calling thread, so
/// it runs off the async threads. A failure is logged and the next pass tries again.
pub async fn retain_once(store: &Store, clock: &Clock) {
    let now = clock();
    let store = store.clone();
    match tokio::task::spawn_blocking(move || store.retain(now)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, "cannot delete the old rows"),
        Err(error) => tracing::warn!(%error, "the deletion of old rows did not finish"),
    }
}

/// Deletes the old rows at the start and then every `RETENTION_INTERVAL`, until the state
/// owner is gone.
pub async fn run_retention(actor: ActorHandle, store: Store, clock: Clock) {
    let mut ticker = tokio::time::interval(RETENTION_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            () = actor.stopped() => return,
            _ = ticker.tick() => {}
        }
        retain_once(&store, &clock).await;
    }
}

/// Samples on `INTERVAL` until the state owner is gone. A missed tick is skipped, not
/// repeated: a late sample does not become two.
pub async fn run(actor: ActorHandle, store: Store, clock: Clock) {
    let mut ticker = tokio::time::interval(INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            () = actor.stopped() => return,
            _ = ticker.tick() => {}
        }
        if sample_once(&actor, &store, &clock).await.is_err() {
            return;
        }
    }
}
