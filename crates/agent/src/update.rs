//! The agent's own update: pull a signed release, check it, swap the binary and restart the agent
//! unit, and put the previous build back when the new one does not come up.
//!
//! The registrar only says which version to move to. Everything else is checked here, from the
//! built-in key: the signature over the manifest, the checksum of the build, the version (never
//! older than the running one) and the key built into the new binary. The engine is never
//! touched: the only unit restarted is the agent's own.

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use mtpool_protocol::agent::{UpdateOutcome, UpdateStatus};
use mtpool_selfupdate::Paths;
use mtpool_selfupdate::manifest::Version;

use crate::host::CommandRunner;

/// What the updater needs from the registrar.
pub trait Source: Send + Sync {
    /// The version the registrar wants this node on, if it wants one.
    fn offer(
        &self,
        running: &Version,
    ) -> impl Future<Output = Result<Option<String>, String>> + Send;
    /// Downloads `/releases/{name}` into `dest`, at most `max` bytes.
    fn fetch(
        &self,
        name: &str,
        dest: &Path,
        max: u64,
    ) -> impl Future<Output = Result<(), String>> + Send;
    /// Tells the registrar how the update went; true when it took the report.
    fn report(&self, status: &UpdateStatus) -> impl Future<Output = bool> + Send;
}

/// What the updater needs from the machine.
pub trait Host: Send + Sync {
    /// Restarts the agent's own unit. Never an engine.
    fn restart_agent(&self) -> impl Future<Output = io::Result<()>> + Send;
    /// The id of the signing key built into the binary at `path`.
    fn binary_key_id(&self, path: &Path) -> Result<Option<String>, String>;
}

#[derive(Debug, Clone)]
pub struct UpdaterConfig {
    /// `releases` is where downloads land; `state` holds the marker, the rejected list and the
    /// status outbox.
    pub paths: Paths,
    pub bin: PathBuf,
    pub target: String,
    /// The built-in public key.
    pub key: String,
    pub current: Version,
    /// How long a new build has to show it is healthy.
    pub watch: Duration,
    pub probe_every: Duration,
    /// A failed offer is not tried again before this.
    pub retry_after: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checked {
    /// Nothing to do.
    Idle,
    /// The new build is in place and the restart has been asked for.
    Restarting(Version),
    /// The update was refused or failed; the code was reported.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirmed {
    Nothing,
    Updated(Version),
    RolledBack(Version),
}

pub struct AgentUpdater<S, H> {
    cfg: UpdaterConfig,
    source: S,
    host: H,
    /// The offer that failed last, and when: it is not tried again before `retry_after`.
    failed: Mutex<Option<(Version, tokio::time::Instant)>>,
}

const OUTBOX_FILE: &str = "outbox.json";
const MAX_MANIFEST: u64 = 256 * 1024;
const MAX_SIGNATURE: u64 = 4096;
const MAX_KEY_FILE: u64 = 4096;

/// What went wrong with one update, as a code for the admin and whether the release is to be
/// remembered as bad.
struct Refusal {
    code: &'static str,
    remember: bool,
}

fn refuse(code: &'static str) -> Refusal {
    Refusal {
        code,
        remember: false,
    }
}

fn refuse_for_good(code: &'static str) -> Refusal {
    Refusal {
        code,
        remember: true,
    }
}

impl<S: Source, H: Host> AgentUpdater<S, H> {
    pub fn new(cfg: UpdaterConfig, source: S, host: H) -> Self {
        AgentUpdater {
            cfg,
            source,
            host,
            failed: Mutex::new(None),
        }
    }

    fn outbox_path(&self) -> PathBuf {
        self.cfg.paths.state.join(OUTBOX_FILE)
    }

    fn queue(&self, status: &UpdateStatus) {
        let text = serde_json::to_string(status).unwrap_or_default();
        if let Err(e) = mtpool_selfupdate::write_atomic(&self.outbox_path(), &text) {
            tracing::warn!(error = %e, "cannot keep the update report for later");
        }
    }

    fn pending(&self) -> Option<UpdateStatus> {
        let text = std::fs::read_to_string(self.outbox_path()).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Sends the report that is waiting, if any, and drops it once the registrar has taken it.
    /// A report survives restarts, so a rollback done while the agent was down still arrives.
    pub async fn flush_outbox(&self) {
        let Some(status) = self.pending() else {
            return;
        };
        if self.source.report(&status).await {
            let _ = std::fs::remove_file(self.outbox_path());
        }
    }

    fn status(version: &Version, outcome: UpdateOutcome, code: Option<&str>) -> UpdateStatus {
        UpdateStatus {
            version: version.to_string(),
            outcome,
            code: code.map(str::to_owned),
        }
    }

    async fn fail(&self, version: &Version, refusal: Refusal) -> Checked {
        tracing::warn!(%version, code = refusal.code, "agent update refused");
        if refusal.remember
            && let Err(e) = self.cfg.paths.reject(version, refusal.code)
        {
            tracing::warn!(error = %e, "cannot record the refused release");
        }
        *lock(&self.failed) = Some((version.clone(), tokio::time::Instant::now()));
        self.queue(&Self::status(
            version,
            UpdateOutcome::Failed,
            Some(refusal.code),
        ));
        self.flush_outbox().await;
        Checked::Failed(refusal.code.to_owned())
    }

    /// One look at what the registrar wants. When there is a newer release, checks it and, if it
    /// holds up, puts it in place and asks for the restart.
    pub async fn check(&self) -> Checked {
        self.flush_outbox().await;
        if self.pending().is_some() {
            // The registrar has not taken the last report: no new work before it knows.
            return Checked::Idle;
        }
        let offered = match self.source.offer(&self.cfg.current).await {
            Ok(Some(text)) => text,
            Ok(None) => return Checked::Idle,
            Err(e) => {
                tracing::debug!(error = %e, "cannot ask the registrar for an update");
                return Checked::Idle;
            }
        };
        let Some(version) = Version::parse(&offered) else {
            tracing::warn!("the registrar offered a version that is not x.y.z");
            return Checked::Idle;
        };
        let running = match self.cfg.paths.installed() {
            Some(installed) if installed > self.cfg.current => installed,
            _ => self.cfg.current.clone(),
        };
        if version <= running {
            return Checked::Idle;
        }
        if let Some((last, at)) = lock(&self.failed).as_ref()
            && *last == version
            && at.elapsed() < self.cfg.retry_after
        {
            return Checked::Idle;
        }
        if self.cfg.paths.is_rejected(&version) {
            return self.fail(&version, refuse("rejected_before")).await;
        }
        let started = Self::status(&version, UpdateOutcome::Started, None);
        let _ = self.source.report(&started).await;
        match self.apply(&version, &running).await {
            Ok(()) => Checked::Restarting(version),
            Err(refusal) => self.fail(&version, refusal).await,
        }
    }

    async fn download(&self, name: &str, max: u64) -> Result<PathBuf, Refusal> {
        let dest = self.cfg.paths.releases.join(name);
        let _ = std::fs::remove_file(&dest);
        self.source.fetch(name, &dest, max).await.map_err(|e| {
            tracing::warn!(file = name, error = %e, "cannot download");
            refuse("fetch_failed")
        })?;
        Ok(dest)
    }

    async fn apply(&self, offered: &Version, running: &Version) -> Result<(), Refusal> {
        use mtpool_selfupdate::manifest::{self, AGENT_COMPONENT, VerifyError};
        use mtpool_selfupdate::{
            MANIFEST_NAME, MAX_ARTIFACT_BYTES, SIGNATURE_NAME, replace_binary, stage, staged_of,
        };

        let manifest_file = self.download(MANIFEST_NAME, MAX_MANIFEST).await?;
        let signature_file = self.download(SIGNATURE_NAME, MAX_SIGNATURE).await?;
        let bytes = read_small(&manifest_file, MAX_MANIFEST);
        let signature = read_small(&signature_file, MAX_SIGNATURE);
        let _ = std::fs::remove_file(&manifest_file);
        let _ = std::fs::remove_file(&signature_file);
        let (Some(bytes), Some(signature)) = (bytes, signature) else {
            return Err(refuse("fetch_failed"));
        };
        let signature = String::from_utf8(signature).map_err(|_| refuse("bad_signature"))?;
        let manifest = manifest::verify_manifest_with(&self.cfg.key, &bytes, &signature).map_err(
            |e| match e {
                VerifyError::Malformed(_) => refuse("bad_manifest"),
                _ => refuse("bad_signature"),
            },
        )?;
        // The registrar said which version; the signed manifest must say the same one. A
        // registrar (or a path to it) that replays an old signed manifest gets nowhere.
        if manifest.version != *offered {
            return Err(refuse("version_mismatch"));
        }
        if manifest.version <= *running {
            return Err(refuse("downgrade"));
        }
        let artifact = manifest
            .artifact(AGENT_COMPONENT, &self.cfg.target)
            .ok_or_else(|| refuse_for_good("no_build"))?;
        let build = self.download(&artifact.name, MAX_ARTIFACT_BYTES).await?;
        let staged = staged_of(&self.cfg.bin);
        let staging = stage(&build, &staged, &artifact.sha256);
        let _ = std::fs::remove_file(&build);
        if let Err(e) = staging {
            tracing::warn!(error = %e, "the build was not staged");
            return Err(if e.contains("checksum") {
                refuse_for_good("bad_checksum")
            } else {
                refuse("stage_failed")
            });
        }

        // The key the new build trusts must be the one that signs the next release: the current
        // one, or the one this very release announces.
        let current_id = mtpool_protocol::signing::key_id(&self.cfg.key);
        let expected = match manifest.key_artifact() {
            None => current_id,
            Some(announced) => {
                let file = self.download(&announced.name, MAX_KEY_FILE).await;
                let bytes = file.as_ref().ok().and_then(|f| read_small(f, MAX_KEY_FILE));
                if let Ok(f) = &file {
                    let _ = std::fs::remove_file(f);
                }
                let key = bytes.ok_or_else(|| refuse("fetch_failed")).and_then(|b| {
                    manifest::rotated_key(&manifest, &b).map_err(|_| refuse_for_good("bad_key"))
                });
                match key {
                    Ok(key) => mtpool_protocol::signing::key_id(&key),
                    Err(e) => {
                        let _ = std::fs::remove_file(&staged);
                        return Err(e);
                    }
                }
            }
        };
        let built_in = match self.host.binary_key_id(&staged) {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(error = %e, "the new build does not run");
                let _ = std::fs::remove_file(&staged);
                return Err(refuse_for_good("not_runnable"));
            }
        };
        if expected.is_none() || built_in != expected {
            let _ = std::fs::remove_file(&staged);
            return Err(refuse_for_good("key_mismatch"));
        }

        self.cfg
            .paths
            .begin_probation(offered, running)
            .map_err(|_| refuse("state_not_writable"))?;
        if let Err(e) = replace_binary(&self.cfg.bin, &staged) {
            tracing::warn!(error = %e, "cannot replace the binary");
            let _ = std::fs::remove_file(&staged);
            let _ = self.cfg.paths.claim_probation();
            return Err(refuse("swap_failed"));
        }
        tracing::info!(version = %offered, "new build in place, restarting the agent");
        if let Err(e) = self.host.restart_agent().await {
            tracing::warn!(error = %e, "cannot restart the agent");
            // Put the old build back at once; nothing is running the new one.
            let _ = self.rollback().await;
            return Err(refuse("restart_failed"));
        }
        Ok(())
    }

    /// Run by the new process. While a build is on probation and this process is that build:
    /// waits for `healthy` to be true, then confirms it; if it never is, puts the previous
    /// build back. Returns at once when there is nothing on probation for this build.
    pub async fn confirm<F: Future<Output = bool> + Send>(
        &self,
        mut healthy: impl FnMut() -> F + Send,
    ) -> Confirmed {
        let Some(marker) = self.cfg.paths.probation() else {
            return Confirmed::Nothing;
        };
        if marker.version != self.cfg.current {
            return Confirmed::Nothing;
        }
        // Same rule as the registrar's watch: at least one good answer, and no bad one after the
        // first good one.
        let started = tokio::time::Instant::now();
        let mut seen = false;
        let mut failed = false;
        while started.elapsed() < self.cfg.watch {
            if healthy().await {
                seen = true;
            } else if seen {
                failed = true;
                break;
            }
            tokio::time::sleep(self.cfg.probe_every).await;
        }
        if failed || !seen {
            return match self.rollback().await {
                Ok(Some(version)) => Confirmed::RolledBack(version),
                _ => Confirmed::Nothing,
            };
        }
        // The rollback unit may have been quicker: then the marker is gone.
        if self.cfg.paths.claim_probation().is_none() {
            return Confirmed::Nothing;
        }
        if let Err(e) = self.cfg.paths.set_installed(&marker.version) {
            tracing::warn!(error = %e, "cannot record the installed version");
        }
        self.queue(&Self::status(&marker.version, UpdateOutcome::Updated, None));
        self.flush_outbox().await;
        Confirmed::Updated(marker.version)
    }

    /// Puts the previous build back when a new one is on probation. `Ok(None)` when none is.
    /// The rolled-back release is remembered and reported, and the agent unit is restarted.
    pub async fn rollback(&self) -> Result<Option<Version>, String> {
        use mtpool_selfupdate::previous_of;
        if self.cfg.paths.probation().is_none() {
            return Ok(None);
        }
        let prev = previous_of(&self.cfg.bin);
        if !prev.is_file() {
            return Err(format!("there is no previous build at {}", prev.display()));
        }
        // One of the callers gets the marker; the other has nothing to do.
        let Some(marker) = self.cfg.paths.claim_probation() else {
            return Ok(None);
        };
        std::fs::rename(&prev, &self.cfg.bin)
            .map_err(|e| format!("cannot restore the previous build: {e}"))?;
        if let Err(e) = self
            .cfg
            .paths
            .reject(&marker.version, "did not become ready after the update")
        {
            tracing::warn!(error = %e, "cannot record the rolled-back release");
        }
        self.queue(&Self::status(
            &marker.version,
            UpdateOutcome::RolledBack,
            None,
        ));
        self.host
            .restart_agent()
            .await
            .map_err(|e| format!("cannot restart the agent: {e}"))?;
        tracing::warn!(version = %marker.version, back_to = %marker.previous, "rolled back");
        Ok(Some(marker.version))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn read_small(path: &Path, max: u64) -> Option<Vec<u8>> {
    let bytes = std::fs::read(path).ok()?;
    (bytes.len() as u64 <= max).then_some(bytes)
}

/// The real machine: `systemctl` on the agent's own unit.
pub struct SystemHost<R> {
    pub runner: R,
    pub unit: String,
}

impl<R: CommandRunner> Host for SystemHost<R> {
    async fn restart_agent(&self) -> io::Result<()> {
        // A unit that hit its start limit has to be cleared before it will start again.
        let _ = self
            .runner
            .run("systemctl", &["reset-failed", &self.unit], None)
            .await;
        // `--no-block`: the restart stops this very process, and a `systemctl` that waited for it
        // would be killed with the unit (the cgroup) and look like a failure.
        let out = self
            .runner
            .run("systemctl", &["restart", "--no-block", &self.unit], None)
            .await?;
        // The restart stops this very process and its control group, `systemctl` included: a
        // `systemctl` that is killed by SIGTERM means the restart is under way.
        use std::os::unix::process::ExitStatusExt;
        if out.status.success() || out.status.signal() == Some(15) {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "systemctl restart {} failed: {}",
                self.unit, out.status
            )))
        }
    }

    fn binary_key_id(&self, path: &Path) -> Result<Option<String>, String> {
        mtpool_selfupdate::binary_key_id(path)
    }
}

/// The registrar, as the updater sees it. The node token is read when it is needed, so an agent
/// that enrolls after it started is served too.
pub struct RegistrarSource {
    pub client: crate::client::RegistrarClient,
    pub state: crate::state_files::StateFiles,
}

impl RegistrarSource {
    fn token(&self) -> Result<String, String> {
        match self.state.load_credentials() {
            Ok(Some(c)) => Ok(c.node_token.expose().to_owned()),
            Ok(None) => Err("the node is not enrolled".to_owned()),
            Err(e) => Err(e.to_string()),
        }
    }
}

impl Source for RegistrarSource {
    async fn offer(&self, running: &Version) -> Result<Option<String>, String> {
        use crate::client::Answer;
        let token = self.token()?;
        let poll = mtpool_protocol::agent::UpdatePoll {
            version: running.to_string(),
        };
        match self.client.update(&token, &poll).await {
            Answer::Ok(offer) => Ok(offer.version),
            other => Err(format!("{other:?}")),
        }
    }

    async fn fetch(&self, name: &str, dest: &Path, max: u64) -> Result<(), String> {
        self.client.release_file(name, dest, max).await
    }

    async fn report(&self, status: &UpdateStatus) -> bool {
        use crate::client::Answer;
        let Ok(token) = self.token() else {
            return false;
        };
        matches!(
            self.client.update_status(&token, status).await,
            Answer::Ok(_)
        )
    }
}

pub const DEFAULT_BIN: &str = "/usr/local/bin/mtpool-agent";
pub const ENV_BIN: &str = "MTPOOL_AGENT_BIN";
pub const UNIT: &str = "mtpool-agent";
pub const ENV_POLL_SEC: &str = "MTPOOL_AGENT_UPDATE_POLL_SEC";
pub const DEFAULT_POLL: Duration = Duration::from_secs(30);

/// The updater's settings on this host, or `None` when this build cannot verify releases (no key
/// built in).
pub fn host_config(state_dir: &Path) -> Option<UpdaterConfig> {
    if crate::MINISIGN_PUB.trim().is_empty() {
        return None;
    }
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let root = state_dir.join("update");
    Some(UpdaterConfig {
        paths: Paths {
            releases: root.join("download"),
            state: root.clone(),
        },
        bin: PathBuf::from(env(ENV_BIN).unwrap_or_else(|| DEFAULT_BIN.to_owned())),
        target: format!("{}-unknown-linux-musl", std::env::consts::ARCH),
        key: crate::MINISIGN_PUB.trim().to_owned(),
        current: Version::parse(env!("CARGO_PKG_VERSION"))?,
        watch: Duration::from_secs(60),
        probe_every: Duration::from_secs(2),
        retry_after: Duration::from_secs(300),
    })
}

/// The poll interval: `MTPOOL_AGENT_UPDATE_POLL_SEC` or 30 s.
pub fn poll_every() -> Duration {
    std::env::var(ENV_POLL_SEC)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map_or(DEFAULT_POLL, Duration::from_secs)
}

/// The background task: confirm a build on probation, then look for a newer release every
/// `every`. Ends when it has asked for a restart: the process is about to be replaced.
pub async fn run_loop<S: Source, H: Host>(updater: AgentUpdater<S, H>, every: Duration) {
    let running = updater.cfg.current.clone();
    let confirmed = updater
        .confirm(|| async { updater.source.offer(&running).await.is_ok() })
        .await;
    tracing::info!(?confirmed, "update probation checked");
    loop {
        match updater.check().await {
            Checked::Restarting(_) => return,
            Checked::Idle | Checked::Failed(_) => {}
        }
        tokio::time::sleep(every).await;
    }
}

#[cfg(test)]
mod tests;
