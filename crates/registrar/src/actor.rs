//! The state owner: one task owns `PoolState`, applies commands with `mtpool_core::apply` and
//! hands the effects on.
//!
//! - `Persist` and `Emit` go to the store (`Store::send` never blocks).
//! - `RunRound`, `PushDns` and `Emit` also go to every `EffectDriver` that accepts them. Each
//!   effect runs in its own task, so a slow driver never delays a command; the driver reports
//!   back with a new command (`RoundDone`, `DnsDone`) through the `ActorHandle`. Drivers bound
//!   their own concurrency; `core` already allows one push per domain and one round per node
//!   at a time.
//! - `Reply` answers the `call` that is waiting for it.
//!
//! Commands that wait in the queue are applied together and one snapshot is published for the
//! batch (rendering the JSON is linear in the pool size). Replies are sent after the snapshot,
//! so a caller that gets its answer also sees the change in the snapshot.
//!
//! Restart grace: no `Tick` goes to `core` during the first heartbeat TTL after start, so a
//! node whose heartbeat is stale in the database gets one TTL to send a new one before it can
//! be declared missing.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mtpool_core::{Command, Effect, PoolState, Reply, ReplyId, Ts, apply};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, MissedTickBehavior};

use crate::snapshot::{Snapshot, SnapshotStore};
use crate::store::Store;

/// How often `Command::Tick` (master selection, node timers, DNS planning) goes to `core`.
pub const SELECTION_TICK: Duration = Duration::from_secs(3);
/// How often the wall clock is compared with the monotonic one.
pub const CLOCK_CHECK_TICK: Duration = Duration::from_secs(30);
/// A wall clock that moved this far against the monotonic one is worth a warning.
pub const CLOCK_JUMP_WARN_MS: i64 = 60_000;

const QUEUE_CAPACITY: usize = 4096;
/// Commands applied before the snapshot is published and the replies are sent.
const MAX_BATCH: usize = 256;

pub type Clock = Arc<dyn Fn() -> Ts + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(|| {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        Ts(i64::try_from(ms).unwrap_or(i64::MAX))
    })
}

pub type DriverFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Performs the effects that touch the outside world. Sensor rounds, DNS and webhooks plug in
/// here. The actor never waits for a driver.
pub trait EffectDriver: Send + Sync + 'static {
    /// Whether this driver wants the effect. Only `RunRound`, `PushDns` and `Emit` are offered.
    fn handles(&self, effect: &Effect) -> bool;

    /// Does the work and reports the outcome as a command through `results`. The returned
    /// future is spawned on its own task.
    fn run(&self, effect: Effect, results: ActorHandle) -> DriverFuture;
}

/// The actor is gone (stopped, or its task ended).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the state owner is not running")]
pub struct ActorStopped;

#[derive(Clone)]
pub struct ActorOptions {
    pub clock: Clock,
    pub selection_tick: Duration,
    pub clock_check_tick: Duration,
}

impl Default for ActorOptions {
    fn default() -> ActorOptions {
        ActorOptions {
            clock: system_clock(),
            selection_tick: SELECTION_TICK,
            clock_check_tick: CLOCK_CHECK_TICK,
        }
    }
}

/// A read of the live state, run by the actor between commands.
type Inspection = Box<dyn FnOnce(&PoolState) + Send>;

enum Msg {
    Command(Command),
    Call(Command, oneshot::Sender<Reply>),
    Inspect(Inspection),
    Stop(oneshot::Sender<()>),
}

/// The way to the actor. Clones share it.
#[derive(Clone)]
pub struct ActorHandle {
    tx: mpsc::Sender<Msg>,
    snapshots: SnapshotStore,
}

impl ActorHandle {
    /// Applies the command and returns its answer. The `reply` id inside the command is
    /// replaced, pass any. A command that has no answer of its own (a tick, a worker result)
    /// gives `Reply::Ok` once applied.
    pub async fn call(&self, command: Command) -> Result<Reply, ActorStopped> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Msg::Call(command, tx))
            .await
            .map_err(|_| ActorStopped)?;
        rx.await.map_err(|_| ActorStopped)
    }

    /// Queues the command without waiting for its result. Waits only while the queue is full.
    pub async fn send(&self, command: Command) -> Result<(), ActorStopped> {
        self.tx
            .send(Msg::Command(command))
            .await
            .map_err(|_| ActorStopped)
    }

    /// Runs `read` on the live state, after the commands queued before it and before the ones
    /// after. For reads that need what the snapshot does not carry, such as a node's
    /// generation. `read` must be quick: it holds the actor.
    pub async fn inspect<T: Send + 'static>(
        &self,
        read: impl FnOnce(&PoolState) -> T + Send + 'static,
    ) -> Result<T, ActorStopped> {
        let (to, rx) = oneshot::channel();
        let inspection: Inspection = Box::new(move |state| {
            let _ = to.send(read(state));
        });
        self.tx
            .send(Msg::Inspect(inspection))
            .await
            .map_err(|_| ActorStopped)?;
        rx.await.map_err(|_| ActorStopped)
    }

    pub fn snapshots(&self) -> SnapshotStore {
        self.snapshots.clone()
    }

    /// The actor no longer takes commands.
    pub fn is_stopped(&self) -> bool {
        self.tx.is_closed()
    }

    /// Completes when the actor is gone. For long-lived connections that must end with it.
    pub async fn stopped(&self) {
        self.tx.closed().await;
    }

    /// Stops the actor after the commands queued before this call and waits until it is gone.
    /// Idempotent.
    pub async fn stop(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Msg::Stop(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }
}

pub struct Actor {
    state: PoolState,
    store: Store,
    drivers: Vec<Arc<dyn EffectDriver>>,
    snapshots: SnapshotStore,
    clock: Clock,
    me: mpsc::WeakSender<Msg>,
    next_reply: u64,
}

impl Actor {
    /// Starts the actor on the current tokio runtime. `state` comes from `Store::load`.
    pub fn spawn(
        state: PoolState,
        store: Store,
        drivers: Vec<Arc<dyn EffectDriver>>,
    ) -> ActorHandle {
        Actor::spawn_with(state, store, drivers, ActorOptions::default())
    }

    pub fn spawn_with(
        mut state: PoolState,
        store: Store,
        drivers: Vec<Arc<dyn EffectDriver>>,
        options: ActorOptions,
    ) -> ActorHandle {
        let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
        let snapshots = SnapshotStore::new(Snapshot::build(&state));
        let grace = u64::from(state.settings.heartbeat_ttl_sec);
        let grace_until = Instant::now() + Duration::from_secs(grace);
        state.evictions_from = Some(Ts((options.clock)()
            .0
            .saturating_add(i64::try_from(grace * 1000).unwrap_or(i64::MAX))));
        let actor = Actor {
            state,
            store,
            drivers,
            snapshots: snapshots.clone(),
            clock: Arc::clone(&options.clock),
            me: tx.downgrade(),
            next_reply: 1,
        };
        tokio::spawn(actor.run(rx, options, grace_until));
        ActorHandle { tx, snapshots }
    }

    async fn run(
        mut self,
        mut rx: mpsc::Receiver<Msg>,
        options: ActorOptions,
        grace_until: Instant,
    ) {
        let mut tick = tokio::time::interval(options.selection_tick);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut clock_check = tokio::time::interval(options.clock_check_tick);
        clock_check.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut last_check = (self.now(), Instant::now());

        loop {
            let mut stop = Vec::new();
            let mut replies = Vec::new();
            tokio::select! {
                msg = rx.recv() => {
                    let Some(msg) = msg else { break };
                    self.accept(msg, &mut replies, &mut stop);
                    let mut batched = 1;
                    while batched < MAX_BATCH && stop.is_empty() {
                        let Ok(msg) = rx.try_recv() else { break };
                        self.accept(msg, &mut replies, &mut stop);
                        batched += 1;
                    }
                }
                _ = tick.tick() => {
                    if Instant::now() >= grace_until {
                        self.process(Command::Tick, None, &mut replies);
                    }
                }
                _ = clock_check.tick() => {
                    let (wall, mono) = (self.now(), Instant::now());
                    let jump = clock_jump_ms(last_check.0, wall, mono - last_check.1);
                    if jump.abs() > CLOCK_JUMP_WARN_MS {
                        tracing::warn!(jump_ms = jump, "wall clock jumped against the monotonic clock");
                    }
                    last_check = (wall, mono);
                }
            }
            self.snapshots.publish(Snapshot::build(&self.state));
            for (to, reply) in replies {
                let _ = to.send(reply);
            }
            if !stop.is_empty() {
                for ack in stop {
                    let _ = ack.send(());
                }
                break;
            }
        }
    }

    fn now(&self) -> Ts {
        (self.clock)()
    }

    fn accept(
        &mut self,
        msg: Msg,
        replies: &mut Vec<(oneshot::Sender<Reply>, Reply)>,
        stop: &mut Vec<oneshot::Sender<()>>,
    ) {
        match msg {
            Msg::Command(command) => {
                self.process(command, None, replies);
            }
            Msg::Call(command, to) => {
                self.process(command, Some(to), replies);
            }
            Msg::Inspect(read) => read(&self.state),
            Msg::Stop(ack) => stop.push(ack),
        }
    }

    fn process(
        &mut self,
        command: Command,
        caller: Option<oneshot::Sender<Reply>>,
        replies: &mut Vec<(oneshot::Sender<Reply>, Reply)>,
    ) {
        let id = ReplyId(self.next_reply);
        self.next_reply += 1;
        let (command, expects_reply) = if caller.is_some() {
            claim_reply_id(command, id)
        } else {
            (command, false)
        };
        let now = self.now();
        let effects = apply(&mut self.state, command, now);
        let mut answer = None;
        for effect in effects {
            match effect {
                Effect::Persist(op) => self.store.send(op),
                Effect::Reply(rid, reply) => {
                    if rid == id {
                        answer = Some(reply);
                    }
                }
                other => {
                    if let Effect::Emit(event) = &other {
                        self.store.send(event.clone());
                    }
                    self.dispatch(other);
                }
            }
        }
        if expects_reply && answer.is_none() {
            tracing::error!(?id, "a command with a reply id produced no reply");
        }
        if let Some(to) = caller {
            replies.push((to, answer.unwrap_or(Reply::Ok)));
        }
    }

    fn dispatch(&self, effect: Effect) {
        let Some(tx) = self.me.upgrade() else { return };
        for driver in self.drivers.iter().filter(|d| d.handles(&effect)) {
            let results = ActorHandle {
                tx: tx.clone(),
                snapshots: self.snapshots.clone(),
            };
            tokio::spawn(driver.run(effect.clone(), results));
        }
    }
}

/// How far the wall clock moved beyond the monotonic time that passed, in milliseconds.
/// Negative: the wall clock went back or stood still.
pub fn clock_jump_ms(prev_wall: Ts, wall: Ts, monotonic_elapsed: Duration) -> i64 {
    let mono = i64::try_from(monotonic_elapsed.as_millis()).unwrap_or(i64::MAX);
    (wall.0 - prev_wall.0).saturating_sub(mono)
}

/// Puts `id` into the command if it carries a reply id; the flag tells whether it does.
fn claim_reply_id(command: Command, id: ReplyId) -> (Command, bool) {
    let command = match command {
        Command::Enroll(mut r) => {
            r.reply = id;
            Command::Enroll(r)
        }
        Command::Heartbeat(mut r) => {
            r.reply = id;
            Command::Heartbeat(r)
        }
        Command::Report(mut r) => {
            r.reply = id;
            Command::Report(r)
        }
        Command::Retire(mut a) => {
            a.reply = id;
            Command::Retire(a)
        }
        Command::AdminUnban(mut a) => {
            a.reply = id;
            Command::AdminUnban(a)
        }
        Command::Recheck(mut a) => {
            a.reply = id;
            Command::Recheck(a)
        }
        Command::SrmdManual(mut r) => {
            r.reply = id;
            Command::SrmdManual(r)
        }
        other => return (other, false),
    };
    (command, true)
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use mtpool_core::{
        EngineKind, EnrollRequest, Generation, HeartbeatRequest, NodeAction, NodeId, OwnerId,
        ReportRequest, SrmdManualAction, SrmdManualRequest, Vote,
    };

    use super::*;

    #[test]
    fn claim_reply_id_covers_every_command_with_a_reply() {
        let node = NodeId::parse("alpha-00001").unwrap();
        let action = || NodeAction {
            reply: ReplyId(0),
            node: node.clone(),
        };
        let with_reply = [
            Command::Enroll(EnrollRequest {
                reply: ReplyId(0),
                node_id: node.clone(),
                owner: OwnerId(1),
                ip: Ipv4Addr::LOCALHOST,
                port: 443,
                engine: EngineKind::Telemt,
                limit: None,
            }),
            Command::Heartbeat(HeartbeatRequest {
                reply: ReplyId(0),
                node: node.clone(),
                ip: Ipv4Addr::LOCALHOST,
            }),
            Command::Report(ReportRequest {
                reply: ReplyId(0),
                node: node.clone(),
                generation: Generation(0),
                metrics_ok: true,
                globalping: Vote::Clear,
                traffic: None,
                clients: None,
                other_traffic: None,
                other_clients: None,
            }),
            Command::Retire(action()),
            Command::AdminUnban(action()),
            Command::Recheck(action()),
            Command::SrmdManual(SrmdManualRequest {
                reply: ReplyId(0),
                domain: "d.example.com".to_owned(),
                action: SrmdManualAction::Take,
            }),
        ];
        for command in with_reply {
            let (claimed, carries) = claim_reply_id(command.clone(), ReplyId(77));
            assert!(carries, "{command:?}");
            assert_ne!(claimed, command, "the reply id was not replaced");
        }
        for command in [Command::Tick, Command::SettingsChanged(Default::default())] {
            assert!(!claim_reply_id(command, ReplyId(77)).1);
        }
    }
}
