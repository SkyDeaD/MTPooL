//! The life of the agent: enroll, then heartbeat, sync the config and report; stay silent when
//! the registrar asks, stop reporting when it terminates the node, enroll again when it forgets
//! the node. [`Lifecycle::step`] does one piece of work and says how long to wait; time is a
//! parameter, so tests drive it without sleeping.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mtpool_protocol::agent::{AgentConfig, Engine, EnrollRequest, Intervals, ReportRequest};
use rand::{RngExt, SeedableRng};
use tokio::time::Instant;

use crate::client::{Answer, RegistrarClient};
use crate::config::LocalConfig;
use crate::state_files::{Credentials, Fs, Secret, StateFiles};

/// How often a terminated node tries to enroll again: that is its reverify.
pub const TERMINATED_RETRY: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentPhase {
    /// No node token: enrolling with the enroll token until it succeeds.
    Enrolling,
    /// Enrolled: heartbeat, config sync and reports.
    Active,
    /// The registrar answered 403 terminate: no reports, the engine untouched.
    Terminated,
    /// The registrar answered 429: nothing is sent until `Retry-After` has passed.
    Silent,
}

/// Why the agent ends instead of retrying.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StopReason {
    #[error(
        "the registrar refused the enroll token in {path} (revoked or unknown): put a new token in that file and start mtpool-agent again"
    )]
    EnrollTokenRevoked { path: PathBuf },
    #[error(
        "there is no enroll token in {path}: the installer writes it, put one there and start mtpool-agent again"
    )]
    EnrollTokenMissing { path: PathBuf },
    #[error("cannot read or write {path}: {message}")]
    StateUnreadable { path: PathBuf, message: String },
}

impl StopReason {
    /// The process exit code: 2 when the operator has to put in a token, 1 for a broken state
    /// directory.
    pub fn exit_code(&self) -> u8 {
        match self {
            StopReason::EnrollTokenRevoked { .. } | StopReason::EnrollTokenMissing { .. } => 2,
            StopReason::StateUnreadable { .. } => 1,
        }
    }
}

/// What to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Call `step` again after this long.
    Wait(Duration),
    /// Stop the agent.
    Stop(StopReason),
}

/// A source of the randomness behind the backoff, in `[0.5, 1.5]`.
pub trait Jitter: Send {
    fn factor(&mut self) -> f64;
}

/// Jitter drawn from a random number generator.
pub struct RngJitter<R>(pub R);

impl<R: rand::Rng + Send> Jitter for RngJitter<R> {
    fn factor(&mut self) -> f64 {
        self.0.random_range(0.5..=1.5)
    }
}

/// Jitter from the operating system's entropy, seeded once.
pub fn system_jitter() -> Box<dyn Jitter> {
    Box::new(RngJitter(rand::rngs::StdRng::from_rng(&mut rand::rng())))
}

/// 5 s, doubling, at most 5 min, then multiplied by the jitter (so up to 7.5 min).
#[derive(Debug, Clone, Default)]
pub struct Backoff {
    attempt: u32,
}

impl Backoff {
    pub const BASE: Duration = Duration::from_secs(5);
    pub const CAP: Duration = Duration::from_secs(300);

    pub fn new() -> Backoff {
        Backoff::default()
    }
    /// The delay before jitter for the next failure.
    pub fn base_delay(&self) -> Duration {
        // Past 6 doublings the cap has been reached anyway; the shift stays in range.
        let doubled = Self::BASE.saturating_mul(1u32 << self.attempt.min(16));
        doubled.min(Self::CAP)
    }
    /// The delay after a failure; the next one is longer. A jitter outside `[0.5, 1.5]` (or not a
    /// number) is brought back into it.
    pub fn next_delay(&mut self, jitter: &mut dyn Jitter) -> Duration {
        let factor = jitter.factor();
        let factor = if factor.is_finite() {
            factor.clamp(0.5, 1.5)
        } else {
            1.0
        };
        let delay = self.base_delay().mul_f64(factor);
        self.attempt = self.attempt.saturating_add(1);
        delay
    }
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

/// The engine the node runs and the port it listens on. Task 21's adapter implements this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetectedEngine {
    pub engine: Engine,
    pub port: u16,
}

pub trait EngineProbe: Send + Sync {
    /// `None` if no engine is found yet.
    fn detect(&self) -> impl Future<Output = Option<DetectedEngine>> + Send;
}

/// An engine fixed in advance.
#[derive(Debug, Clone, Copy)]
pub struct StaticEngine(pub Option<DetectedEngine>);

impl EngineProbe for StaticEngine {
    async fn detect(&self) -> Option<DetectedEngine> {
        self.0
    }
}

/// Builds the report of the round; Task 23 implements it.
pub trait ReportSource: Send + Sync {
    /// `None`: nothing to report now.
    fn build(&self) -> impl Future<Output = Option<ReportRequest>> + Send;
}

/// Reports nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoReports;

impl ReportSource for NoReports {
    async fn build(&self) -> Option<ReportRequest> {
        None
    }
}

/// Receives the config of the node after every sync; Task 21's engine adapter applies it.
pub trait ConfigSink: Send + Sync {
    fn apply(&self, config: &AgentConfig) -> impl Future<Output = ()> + Send;
}

/// Logs what the config holds (never the secrets).
#[derive(Debug, Clone, Copy, Default)]
pub struct LogSink;

impl ConfigSink for LogSink {
    async fn apply(&self, config: &AgentConfig) {
        tracing::info!(
            users = config.users.len(),
            port = config.port,
            sni = %config.sni,
            antiscan = config.antiscan,
            "config of the node received"
        );
    }
}

/// The parts the lifecycle drives.
pub struct Hooks<E, S, K> {
    pub engine: E,
    pub reports: S,
    pub sink: K,
}

/// One recurring piece of work: when it is next due and how it backs off after failures.
#[derive(Debug, Default)]
struct Job {
    /// `None`: due at once.
    next_at: Option<Instant>,
    backoff: Backoff,
}

impl Job {
    fn due(&self, now: Instant) -> bool {
        self.next_at.is_none_or(|at| now >= at)
    }
    fn succeeded(&mut self, now: Instant, interval_sec: u32) {
        self.backoff.reset();
        self.next_at = Some(now + seconds(interval_sec));
    }
    fn failed(&mut self, now: Instant, jitter: &mut dyn Jitter) {
        self.next_at = Some(now + self.backoff.next_delay(jitter));
    }
    /// A refusal that is not worth a backoff: try again at the usual time.
    fn skipped(&mut self, now: Instant, interval_sec: u32) {
        self.next_at = Some(now + seconds(interval_sec));
    }
    fn restart(&mut self) {
        *self = Job::default();
    }
}

fn seconds(n: u32) -> Duration {
    Duration::from_secs(u64::from(n.max(1)))
}

/// What a node route answered, as the lifecycle sees it.
enum Outcome<T> {
    Ok(T),
    /// A failure; `server` if it is the registrar's or the network's fault, not a refusal.
    Failed {
        server: bool,
    },
    /// The answer changed the phase (or stopped the agent): hand this step back.
    Leave(Step),
}

pub struct Lifecycle<E, S, K> {
    hooks: Hooks<E, S, K>,
    client: RegistrarClient,
    state: StateFiles,
    name: String,
    credentials: Option<Credentials>,
    intervals: Intervals,
    jitter: Box<dyn Jitter>,
    phase: AgentPhase,
    /// The phase to return to when the silence is over.
    resume: AgentPhase,
    silent_until: Option<Instant>,
    /// When a terminated node tries to enroll again.
    retry_at: Option<Instant>,
    enroll: Job,
    heartbeat: Job,
    sync: Job,
    report: Job,
}

impl<E: EngineProbe, S: ReportSource, K: ConfigSink> Lifecycle<E, S, K> {
    /// Reads the state files: with a node id and token the agent starts `Active`, otherwise
    /// `Enrolling`.
    pub fn new(
        config: &LocalConfig,
        client: RegistrarClient,
        fs: Arc<dyn Fs>,
        hooks: Hooks<E, S, K>,
        jitter: Box<dyn Jitter>,
    ) -> std::io::Result<Self> {
        let state = StateFiles::new(fs, config.state_dir.clone());
        let credentials = state.load_credentials()?;
        let phase = if credentials.is_some() {
            AgentPhase::Active
        } else {
            AgentPhase::Enrolling
        };
        Ok(Lifecycle {
            hooks,
            client,
            state,
            name: config.name.clone(),
            credentials,
            intervals: config.intervals(),
            jitter,
            phase,
            resume: phase,
            silent_until: None,
            retry_at: None,
            enroll: Job::default(),
            heartbeat: Job::default(),
            sync: Job::default(),
            report: Job::default(),
        })
    }

    pub fn phase(&self) -> AgentPhase {
        self.phase
    }

    /// The intervals in force.
    pub fn intervals(&self) -> Intervals {
        self.intervals
    }

    /// Does the work that is due at `now` and says how long to wait.
    pub async fn step(&mut self, now: Instant) -> Step {
        if self.phase == AgentPhase::Silent {
            match self.silent_until {
                Some(until) if now < until => return Step::Wait(until - now),
                _ => {
                    self.phase = self.resume;
                    self.silent_until = None;
                }
            }
        }
        match self.phase {
            AgentPhase::Enrolling => match self.enroll.next_at {
                Some(at) if now < at => Step::Wait(at - now),
                _ => self.try_enroll(now).await,
            },
            AgentPhase::Terminated => match self.retry_at {
                Some(at) if now < at => Step::Wait(at - now),
                _ => self.try_enroll(now).await,
            },
            AgentPhase::Active => self.work(now).await,
            // Cannot happen: the silence was ended above.
            AgentPhase::Silent => Step::Wait(Duration::from_secs(1)),
        }
    }

    /// Runs until `shutdown` completes (`Ok`; a shutdown is not a departure, nothing is sent)
    /// or until the agent has to stop (`Err`).
    pub async fn run(mut self, shutdown: impl Future<Output = ()>) -> Result<(), StopReason> {
        tokio::pin!(shutdown);
        loop {
            // Cutting a step short is safe: the state files are written without awaiting.
            let step = tokio::select! {
                biased;
                () = &mut shutdown => return Ok(()),
                step = self.step(Instant::now()) => step,
            };
            match step {
                Step::Stop(reason) => return Err(reason),
                Step::Wait(delay) => tokio::select! {
                    biased;
                    () = &mut shutdown => return Ok(()),
                    () = tokio::time::sleep(delay) => {}
                },
            }
        }
    }

    fn state_error(&self, path: PathBuf, error: &std::io::Error) -> Step {
        Step::Stop(StopReason::StateUnreadable {
            path,
            message: error.to_string(),
        })
    }

    fn go_silent(&mut self, now: Instant, retry_after: Duration) -> Step {
        tracing::warn!(
            seconds = retry_after.as_secs(),
            "the registrar asks for silence"
        );
        self.resume = self.phase;
        self.phase = AgentPhase::Silent;
        self.silent_until = Some(now + retry_after);
        Step::Wait(retry_after)
    }

    fn terminated(&mut self, now: Instant, reason: &impl std::fmt::Debug) -> Step {
        tracing::warn!(
            ?reason,
            "the registrar terminated this node: reports stop, the engine is left as it is"
        );
        self.phase = AgentPhase::Terminated;
        self.retry_at = Some(now + TERMINATED_RETRY);
        Step::Wait(TERMINATED_RETRY)
    }

    /// Back to enrolling; `forget_id` if the registrar does not know the node at all.
    fn start_enrolling(&mut self, forget_id: bool) -> Step {
        let removed = if forget_id {
            self.state
                .clear_credentials()
                .map_err(|e| (self.state.node_token_path(), e))
        } else {
            self.state
                .remove_node_token()
                .map_err(|e| (self.state.node_token_path(), e))
        };
        if let Err((path, error)) = removed {
            return self.state_error(path, &error);
        }
        self.credentials = None;
        self.phase = AgentPhase::Enrolling;
        self.enroll.restart();
        Step::Wait(Duration::ZERO)
    }

    /// One enroll attempt, for a node that is enrolling or terminated.
    async fn try_enroll(&mut self, now: Instant) -> Step {
        let was_terminated = self.phase == AgentPhase::Terminated;
        let token_path = self.state.enroll_token_path();
        let token: Secret = match self.state.enroll_token() {
            Ok(Some(token)) => token,
            Ok(None) => return Step::Stop(StopReason::EnrollTokenMissing { path: token_path }),
            Err(error) => return self.state_error(token_path, &error),
        };
        let Some(detected) = self.hooks.engine.detect().await else {
            tracing::warn!("no proxy engine found yet; waiting");
            return self.enroll_failed(now, was_terminated);
        };
        let request = EnrollRequest {
            name: self.name.clone(),
            port: detected.port,
            engine: detected.engine,
        };
        match self.client.enroll(token.expose(), &request).await {
            Answer::Ok(response) => {
                let credentials = Credentials {
                    node_id: response.node_id,
                    node_token: Secret::new(response.node_token),
                };
                if let Err(error) = self.state.save_credentials(&credentials) {
                    return self.state_error(self.state.node_token_path(), &error);
                }
                tracing::info!(node_id = %credentials.node_id, "enrolled");
                self.credentials = Some(credentials);
                self.phase = AgentPhase::Active;
                self.retry_at = None;
                self.enroll.restart();
                self.heartbeat.restart();
                self.sync.restart();
                self.report.restart();
                Step::Wait(Duration::ZERO)
            }
            Answer::Unauthorized => Step::Stop(StopReason::EnrollTokenRevoked { path: token_path }),
            Answer::Terminate(document) => self.terminated(now, &document.reason),
            Answer::Paused { retry_after } => self.go_silent(now, retry_after),
            Answer::Unknown | Answer::Problem(_) | Answer::Transport(_) => {
                self.enroll_failed(now, was_terminated)
            }
        }
    }

    fn enroll_failed(&mut self, now: Instant, was_terminated: bool) -> Step {
        if was_terminated {
            self.retry_at = Some(now + TERMINATED_RETRY);
            return Step::Wait(TERMINATED_RETRY);
        }
        let delay = self.enroll.backoff.next_delay(&mut *self.jitter);
        self.enroll.next_at = Some(now + delay);
        tracing::warn!(
            seconds = delay.as_secs(),
            "enroll failed; trying again later"
        );
        Step::Wait(delay)
    }

    /// Sorts an answer of a node route into what the lifecycle does with it.
    fn outcome<T>(&mut self, now: Instant, answer: Answer<T>) -> Outcome<T> {
        match answer {
            Answer::Ok(value) => Outcome::Ok(value),
            Answer::Unauthorized => {
                tracing::warn!("the registrar refused the node token; enrolling again");
                Outcome::Leave(self.start_enrolling(false))
            }
            Answer::Unknown => {
                tracing::warn!("the registrar does not know this node; enrolling again");
                Outcome::Leave(self.start_enrolling(true))
            }
            Answer::Terminate(document) => Outcome::Leave(self.terminated(now, &document.reason)),
            Answer::Paused { retry_after } => Outcome::Leave(self.go_silent(now, retry_after)),
            Answer::Problem(problem) => {
                tracing::warn!(status = problem.status, code = %problem.code, "the registrar refused a request");
                Outcome::Failed {
                    server: problem.status >= 500,
                }
            }
            Answer::Transport(message) => {
                tracing::warn!(%message, "cannot reach the registrar");
                Outcome::Failed { server: true }
            }
        }
    }

    /// The jobs of an enrolled node: heartbeat, config sync, report.
    async fn work(&mut self, now: Instant) -> Step {
        let Some(token) = self
            .credentials
            .as_ref()
            .map(|c| c.node_token.expose().to_owned())
        else {
            self.phase = AgentPhase::Enrolling;
            return Step::Wait(Duration::ZERO);
        };

        if self.heartbeat.due(now) {
            let answer = self.client.heartbeat(&token).await;
            match self.outcome(now, answer) {
                Outcome::Ok(_) => self.heartbeat.succeeded(now, self.intervals.heartbeat_sec),
                Outcome::Failed { .. } => self.heartbeat.failed(now, &mut *self.jitter),
                Outcome::Leave(step) => return step,
            }
        }

        if self.sync.due(now) {
            let answer = self.client.config(&token).await;
            match self.outcome(now, answer) {
                Outcome::Ok(config) => {
                    self.intervals = sane(config.intervals);
                    self.sync.succeeded(now, self.intervals.sync_sec);
                    self.hooks.sink.apply(&config).await;
                }
                Outcome::Failed { .. } => self.sync.failed(now, &mut *self.jitter),
                Outcome::Leave(step) => return step,
            }
        }

        if self.report.due(now) {
            match self.hooks.reports.build().await {
                None => self.report.succeeded(now, self.intervals.metrics_sec),
                Some(request) => {
                    let answer = self.client.report(&token, &request).await;
                    match self.outcome(now, answer) {
                        Outcome::Ok(_) => self.report.succeeded(now, self.intervals.metrics_sec),
                        // A refused report (clock skew, a mismatch) is not helped by hurrying.
                        Outcome::Failed { server: false } => {
                            self.report.skipped(now, self.intervals.metrics_sec);
                        }
                        Outcome::Failed { server: true } => {
                            self.report.failed(now, &mut *self.jitter)
                        }
                        Outcome::Leave(step) => return step,
                    }
                }
            }
        }

        let soonest = [&self.heartbeat, &self.sync, &self.report]
            .into_iter()
            .filter_map(|job| job.next_at)
            .min();
        Step::Wait(soonest.map_or(Duration::ZERO, |at| at.saturating_duration_since(now)))
    }
}

/// The registrar's intervals with every zero raised to one second, so a bad config cannot make
/// the agent spin.
fn sane(intervals: Intervals) -> Intervals {
    Intervals {
        heartbeat_sec: intervals.heartbeat_sec.max(1),
        globalping_sec: intervals.globalping_sec.max(1),
        metrics_sec: intervals.metrics_sec.max(1),
        sync_sec: intervals.sync_sec.max(1),
    }
}
