//! The client sampler: every 30 seconds it reads the client numbers off the state owner and
//! queues them for the store, for the history chart.
//!
//! The state owner is asked once through `ActorHandle::inspect` (a quick read); the write is
//! a non-blocking send to the store writer. Nothing here waits for the disk.

use std::time::Duration;

use mtpool_core::{Ts, client_sample};
use tokio::time::MissedTickBehavior;

use crate::actor::{ActorHandle, ActorStopped, Clock};
use crate::store::{HistoryOp, Store};

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
    let sample = actor
        .inspect(move |state| client_sample(state, now))
        .await?;
    store.send(HistoryOp::ClientSamples {
        at: aligned(sample.at),
        rows: sample.rows(),
    });
    Ok(())
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
