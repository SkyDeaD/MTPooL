//! Self-update of the registrar from GitHub Releases.
//!
//! Two halves with different rights. The worker in this file runs inside the registrar service
//! (user `mtpool`, sandboxed): it polls the releases, checks the signature, downloads, checks the
//! checksums and puts the whole release into the releases directory, manifest last. It never
//! touches the binary. A root helper (`apply`, started by a systemd path unit when the manifest
//! changes) checks everything again from the files, swaps the binary, restarts the service,
//! watches `/readyz` and, when the new build does not come up, the rollback unit puts the previous
//! binary back.

pub mod apply;
pub mod github;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

pub use mtpool_selfupdate::manifest;
pub use mtpool_selfupdate::{
    MANIFEST_NAME, MAX_ARTIFACT_BYTES, Paths, Probation, SIGNATURE_NAME, write_atomic,
};

use manifest::{Manifest, VerifyError, Version};

pub const ENV_RELEASE_REPO: &str = "MTPOOL_RELEASE_REPO";
pub const ENV_GITHUB_URL: &str = "MTPOOL_GITHUB_URL";
pub const ENV_GITHUB_DOWNLOAD_URL: &str = "MTPOOL_GITHUB_DOWNLOAD_URL";
pub const ENV_UPDATE_POLL_SECS: &str = "MTPOOL_UPDATE_POLL_SECS";
pub const ENV_UPDATE_STATE_DIR: &str = "MTPOOL_UPDATE_STATE_DIR";
pub const ENV_UPDATE_BIN: &str = "MTPOOL_UPDATE_BIN";

pub const DEFAULT_STATE_DIR: &str = "/var/lib/mtpool-update";
pub const DEFAULT_BIN: &str = "/usr/local/bin/mtpool-registrar";
pub const POLL_EVERY: Duration = Duration::from_secs(300);
/// The longest the poller stays away after GitHub says the limit is used up.
pub const MAX_BACKOFF: Duration = Duration::from_secs(3600);
/// The whole release is never bigger than this (one build: [`MAX_ARTIFACT_BYTES`]).
pub const MAX_RELEASE_BYTES: u64 = 1024 * 1024 * 1024;
pub(crate) const STAGING: &str = ".staging";

/// Something the admin should see: it goes to the log and to the audit log.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub action: &'static str,
    pub target: Option<String>,
    pub detail: Value,
}

pub type Reporter = Arc<dyn Fn(Event) + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    /// The API says the request limit is used up; try again after this long.
    Limited(Duration),
    Failed(String),
}

/// The newest release as the source shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub tag: String,
    pub manifest: Vec<u8>,
    pub signature: String,
    /// Names of the files the release has.
    pub assets: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Latest {
    /// Nothing changed since the last answer.
    Unchanged,
    /// The repository has no release.
    None,
    Release(Offer),
}

#[async_trait]
pub trait ReleaseSource: Send + Sync {
    async fn latest(&self) -> Result<Latest, SourceError>;
    /// Fetches the file `name` of release `tag` into `dest`, at most `max` bytes.
    async fn download(
        &self,
        tag: &str,
        name: &str,
        dest: &Path,
        max: u64,
    ) -> Result<(), SourceError>;
}

/// What one pass of the poller did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Idle,
    Ignored(Version),
    Published(Version),
    Refused(String),
    Failed(String),
    Limited(Duration),
}

pub struct Updater<S> {
    source: S,
    paths: Paths,
    key: String,
    current: Version,
    report: Reporter,
    interval: Duration,
    pending: Option<Offer>,
    refused: HashSet<String>,
    retriggered: HashSet<String>,
    last_failure: Option<String>,
}

impl<S: ReleaseSource> Updater<S> {
    pub fn new(source: S, paths: Paths, key: String, current: Version, report: Reporter) -> Self {
        Updater {
            source,
            paths,
            key,
            current,
            report,
            interval: POLL_EVERY,
            pending: None,
            refused: HashSet::new(),
            retriggered: HashSet::new(),
            last_failure: None,
        }
    }

    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// One poll. Returns what happened and how long to wait before the next one.
    pub async fn step(&mut self) -> (Outcome, Duration) {
        let outcome = match self.source.latest().await {
            Err(SourceError::Limited(wait)) => {
                self.say(
                    "update.rate_limited",
                    None,
                    json!({ "retry_after_secs": wait.as_secs() }),
                );
                let clamped = wait.clamp(self.interval, MAX_BACKOFF);
                return (Outcome::Limited(wait), clamped);
            }
            Err(SourceError::Failed(why)) => self.failed(why),
            Ok(Latest::None) => Outcome::Idle,
            Ok(Latest::Unchanged) => match self.pending.clone() {
                Some(offer) => self.handle(offer).await,
                None => Outcome::Idle,
            },
            Ok(Latest::Release(offer)) => self.handle(offer).await,
        };
        (outcome, self.interval)
    }

    fn say(&self, action: &'static str, target: Option<String>, detail: Value) {
        (self.report)(Event {
            action,
            target,
            detail,
        });
    }

    /// A failure that may pass. Said once per distinct reason, not every five minutes.
    fn failed(&mut self, why: String) -> Outcome {
        if self.last_failure.as_deref() != Some(why.as_str()) {
            self.say("update.failed", None, json!({ "reason": why }));
            self.last_failure = Some(why.clone());
        }
        Outcome::Failed(why)
    }

    fn refuse(&mut self, tag: &str, reason: String) -> Outcome {
        self.refused.insert(tag.to_owned());
        self.pending = None;
        self.say(
            "update.rejected",
            Some(tag.to_owned()),
            json!({ "reason": reason }),
        );
        Outcome::Refused(reason)
    }

    async fn handle(&mut self, offer: Offer) -> Outcome {
        if self.refused.contains(&offer.tag) {
            self.pending = None;
            return Outcome::Idle;
        }
        // Nothing in the offer counts until the signature over these exact bytes is good.
        let manifest =
            match manifest::verify_manifest_with(&self.key, &offer.manifest, &offer.signature) {
                Ok(m) => m,
                Err(e) => return self.refuse(&offer.tag, e.to_string()),
            };
        let running = effective_current(&self.current, &self.paths);
        if manifest.version <= running || self.paths.is_rejected(&manifest.version) {
            self.pending = None;
            return Outcome::Ignored(manifest.version);
        }
        if self.published_manifest().as_deref() == Some(offer.manifest.as_slice()) {
            self.pending = None;
            self.retrigger(&offer);
            return Outcome::Idle;
        }
        match self.fetch_and_publish(&offer, &manifest).await {
            Ok(()) => {
                self.pending = None;
                self.last_failure = None;
                self.say(
                    "update.published",
                    Some(manifest.version.to_string()),
                    json!({ "tag": offer.tag }),
                );
                Outcome::Published(manifest.version)
            }
            Err(Fault::Bad(reason)) => self.refuse(&offer.tag, reason),
            Err(Fault::Passing(why)) => {
                self.pending = Some(offer);
                self.failed(why)
            }
        }
    }

    fn published_manifest(&self) -> Option<Vec<u8>> {
        std::fs::read(self.paths.releases.join(MANIFEST_NAME)).ok()
    }

    /// The helper reacts to the manifest file changing. If it gave up (or was not there yet) while
    /// the release stays the newest, one more nudge per release is allowed, no more.
    fn retrigger(&mut self, offer: &Offer) {
        if !self.retriggered.insert(offer.tag.clone()) {
            return;
        }
        let text = String::from_utf8_lossy(&offer.manifest).into_owned();
        if let Err(e) = write_atomic(&self.paths.releases.join(MANIFEST_NAME), &text) {
            tracing::warn!(error = %e, "cannot nudge the update helper");
        }
    }

    async fn fetch_and_publish(&self, offer: &Offer, manifest: &Manifest) -> Result<(), Fault> {
        use std::os::unix::fs::PermissionsExt;
        let passing =
            |what: &str, e: &dyn std::fmt::Display| Fault::Passing(format!("{what}: {e}"));

        let mut names: Vec<&manifest::Artifact> = manifest.artifacts.iter().collect();
        names.sort_by(|a, b| a.name.cmp(&b.name));
        for artifact in &names {
            if !offer.assets.iter().any(|a| a == &artifact.name) {
                return Err(Fault::Passing(format!(
                    "the release has no file {}",
                    artifact.name
                )));
            }
        }

        let staging = self.paths.releases.join(STAGING);
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging)
            .map_err(|e| passing("cannot create the staging directory", &e))?;
        let result = async {
            let mut left = MAX_RELEASE_BYTES;
            for artifact in &names {
                let dest = staging.join(&artifact.name);
                let max = left.min(MAX_ARTIFACT_BYTES);
                self.source
                    .download(&offer.tag, &artifact.name, &dest, max)
                    .await
                    .map_err(|e| match e {
                        SourceError::Limited(w) => {
                            Fault::Passing(format!("rate limited for {}s", w.as_secs()))
                        }
                        SourceError::Failed(why) => Fault::Passing(why),
                    })?;
                let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
                left = left.saturating_sub(size);
                match manifest::verify_artifact(&dest, &artifact.sha256) {
                    Ok(()) => {}
                    Err(VerifyError::ChecksumMismatch) => {
                        return Err(Fault::Bad(format!(
                            "{} does not match the checksum in the signed manifest",
                            artifact.name
                        )));
                    }
                    Err(e) => return Err(passing("cannot check a download", &e)),
                }
                std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o644))
                    .map_err(|e| passing("cannot set permissions", &e))?;
            }
            // Everything is here and checked. Files first, the manifest last: whoever sees the
            // manifest finds every file it names.
            for artifact in &names {
                std::fs::rename(
                    staging.join(&artifact.name),
                    self.paths.releases.join(&artifact.name),
                )
                .map_err(|e| passing("cannot publish a file", &e))?;
            }
            write_atomic(&self.paths.releases.join(SIGNATURE_NAME), &offer.signature)
                .map_err(|e| passing("cannot publish the signature", &e))?;
            let text = String::from_utf8_lossy(&offer.manifest).into_owned();
            write_atomic(&self.paths.releases.join(MANIFEST_NAME), &text)
                .map_err(|e| passing("cannot publish the manifest", &e))
        }
        .await;
        let _ = std::fs::remove_dir_all(&staging);
        result
    }

    pub async fn run(mut self) {
        loop {
            let (_, wait) = self.step().await;
            tokio::time::sleep(wait).await;
        }
    }
}

/// Why a release did not get published: it is bad for good (never tried again), or the attempt
/// failed in a way that may pass.
enum Fault {
    Bad(String),
    Passing(String),
}

/// The version that counts as running: the one built in, or the one the helper confirmed after an
/// update, whichever is newer.
pub fn effective_current(compiled: &Version, state: &Paths) -> Version {
    match state.installed() {
        Some(installed) if installed > *compiled => installed,
        _ => compiled.clone(),
    }
}

/// The target triple this build runs on, as the release files name it.
pub fn host_target() -> String {
    format!("{}-unknown-linux-musl", std::env::consts::ARCH)
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn paths_from_env() -> Paths {
    Paths {
        releases: crate::config::releases_dir(|n| std::env::var(n).ok()),
        state: PathBuf::from(
            env(ENV_UPDATE_STATE_DIR).unwrap_or_else(|| DEFAULT_STATE_DIR.to_owned()),
        ),
    }
}

pub(crate) fn compiled_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the crate version is a version")
}

/// The poller, when `MTPOOL_RELEASE_REPO` is set; `None` when updating is switched off or cannot
/// be set up. Results reach the admin through the log and the audit log.
pub fn spawn_poller(store: crate::store::Store) -> Option<tokio::task::JoinHandle<()>> {
    let repo = env(ENV_RELEASE_REPO)?;
    if crate::release::MINISIGN_PUB.is_empty() {
        tracing::warn!("this build has no signing key built in, self-update is off");
        return None;
    }
    let url = |name: &str, default: &str| -> Option<reqwest::Url> {
        let text = env(name).unwrap_or_else(|| default.to_owned());
        match reqwest::Url::parse(&text) {
            Ok(url) => Some(url),
            Err(_) => {
                tracing::error!(variable = name, "not a URL, self-update is off");
                None
            }
        }
    };
    let api = url(ENV_GITHUB_URL, github::DEFAULT_API)?;
    let download = url(ENV_GITHUB_DOWNLOAD_URL, github::DEFAULT_DOWNLOAD)?;
    let source = match github::GithubSource::new(api, download, &repo) {
        Ok(source) => source,
        Err(e) => {
            tracing::error!(error = %e, "cannot set up the release client, self-update is off");
            return None;
        }
    };
    let clock = crate::actor::system_clock();
    let report: Reporter = Arc::new(move |event: Event| {
        tracing::warn!(action = event.action, target = ?event.target, detail = %event.detail, "update");
        crate::audit::record(
            &store,
            clock(),
            "system",
            event.action,
            event.target.as_deref(),
            event.detail,
        );
    });
    let paths = paths_from_env();
    let mut updater = Updater::new(
        source,
        paths,
        crate::release::MINISIGN_PUB.to_owned(),
        compiled_version(),
        report,
    );
    // Only a test stand shortens the five minutes.
    if let Some(secs) = env(ENV_UPDATE_POLL_SECS)
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s > 0)
    {
        updater = updater.with_interval(Duration::from_secs(secs));
    }
    tracing::info!(%repo, "self-update is on");
    Some(tokio::spawn(updater.run()))
}

fn apply_config(listen: std::net::SocketAddr) -> (apply::ApplyConfig, apply::SystemHost) {
    let cfg = apply::ApplyConfig {
        paths: paths_from_env(),
        bin: PathBuf::from(env(ENV_UPDATE_BIN).unwrap_or_else(|| DEFAULT_BIN.to_owned())),
        target: host_target(),
        key: crate::release::MINISIGN_PUB.to_owned(),
        current: compiled_version(),
        watch: apply::WATCH_FOR,
        probe_every: apply::PROBE_EVERY,
    };
    let host = apply::SystemHost {
        listen,
        unit: "mtpool-registrar.service".to_owned(),
    };
    (cfg, host)
}

/// `mtpool-registrar update apply`: installs the published release when it is a newer, intact,
/// signed one. Looks again afterwards, in case a newer release was published meanwhile.
pub async fn run_apply(listen: std::net::SocketAddr) -> Result<String, String> {
    let (cfg, host) = apply_config(listen);
    let mut report = String::from("nothing to update");
    for _ in 0..3 {
        // After an update the running build is the new one, so the version to beat moves on.
        let cfg = apply::ApplyConfig {
            current: effective_current(&cfg.current, &cfg.paths),
            ..cfg.clone()
        };
        match apply::apply_self_update(&cfg, &host).await {
            Ok(apply::Applied::NothingToDo) => break,
            Ok(apply::Applied::Updated(v)) => report = format!("updated to {v}"),
            Err(apply::ApplyError::Rejected { version, reason }) => {
                let what =
                    version.map_or_else(|| "the published release".to_owned(), |v| v.to_string());
                return Err(format!("{what} refused: {reason}"));
            }
            Err(apply::ApplyError::WatchFailed(v)) => {
                return Err(format!(
                    "{v} did not become ready in time; it will be rolled back"
                ));
            }
            Err(apply::ApplyError::Superseded(v)) => {
                return Err(format!("{v} was rolled back while it was being watched"));
            }
            Err(apply::ApplyError::Failed(why)) => return Err(why),
        }
    }
    Ok(report)
}

/// `mtpool-registrar update rollback`: puts the previous build back if the last update is on
/// probation.
pub async fn run_rollback(listen: std::net::SocketAddr) -> Result<String, String> {
    let (cfg, host) = apply_config(listen);
    match apply::rollback(&cfg, &host).await {
        Ok(Some(v)) => Ok(format!("rolled back {v}")),
        Ok(None) => Ok("no update on probation, nothing to roll back".to_owned()),
        Err(e) => Err(format!("{e:?}")),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct Fake {
        answers: Mutex<VecDeque<Result<Latest, SourceError>>>,
        files: Mutex<HashMap<String, Vec<u8>>>,
        downloads: Mutex<Vec<String>>,
        calls: AtomicUsize,
    }

    impl Fake {
        fn new(answers: Vec<Result<Latest, SourceError>>) -> Arc<Fake> {
            Arc::new(Fake {
                answers: Mutex::new(answers.into()),
                files: Mutex::new(HashMap::new()),
                downloads: Mutex::new(Vec::new()),
                calls: AtomicUsize::new(0),
            })
        }
    }

    #[async_trait]
    impl ReleaseSource for Arc<Fake> {
        async fn latest(&self) -> Result<Latest, SourceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.answers
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(Latest::Unchanged))
        }
        async fn download(
            &self,
            _tag: &str,
            name: &str,
            dest: &Path,
            _max: u64,
        ) -> Result<(), SourceError> {
            self.downloads.lock().unwrap().push(name.to_owned());
            let body = self
                .files
                .lock()
                .unwrap()
                .get(name)
                .cloned()
                .ok_or_else(|| SourceError::Failed(format!("no {name}")))?;
            std::fs::write(dest, &body).map_err(|e| SourceError::Failed(e.to_string()))
        }
    }

    fn updater(source: Arc<Fake>, dir: &Path) -> (Updater<Arc<Fake>>, Arc<Mutex<Vec<Event>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let paths = Paths {
            releases: dir.join("releases"),
            state: dir.join("state"),
        };
        std::fs::create_dir_all(&paths.releases).unwrap();
        std::fs::create_dir_all(&paths.state).unwrap();
        let u = Updater::new(
            source,
            paths,
            KEY.with(|k| k.pk.to_base64()),
            Version::parse("1.2.3").unwrap(),
            Arc::new(move |e| sink.lock().unwrap().push(e)),
        );
        (u, events)
    }

    #[tokio::test(start_paused = true)]
    async fn poll_every_5min() {
        let dir = tempfile::tempdir().unwrap();
        let source = Fake::new(vec![Ok(Latest::None); 10]);
        let (u, _) = updater(source.clone(), dir.path());
        let task = tokio::spawn(u.run());
        let calls = || source.calls.load(Ordering::SeqCst);
        tokio::task::yield_now().await;
        assert_eq!(calls(), 1, "the first poll is at once");
        tokio::time::advance(Duration::from_secs(299)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls(), 1, "nothing before five minutes");
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls(), 2);
        tokio::time::advance(Duration::from_secs(300)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls(), 3);
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn github_rate_limit_backs_off() {
        let dir = tempfile::tempdir().unwrap();
        let source = Fake::new(vec![
            Err(SourceError::Limited(Duration::from_secs(20 * 60))),
            Err(SourceError::Limited(Duration::from_secs(10 * 3600))),
            Err(SourceError::Limited(Duration::from_secs(1))),
        ]);
        let (mut u, events) = updater(source, dir.path());
        let (outcome, wait) = u.step().await;
        assert_eq!(outcome, Outcome::Limited(Duration::from_secs(1200)));
        assert_eq!(
            wait,
            Duration::from_secs(1200),
            "waits until the limit resets"
        );
        let (_, wait) = u.step().await;
        assert_eq!(
            wait, MAX_BACKOFF,
            "a far reset does not park the poller for hours"
        );
        let (_, wait) = u.step().await;
        assert_eq!(wait, POLL_EVERY, "never polls faster than usual");
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e.action == "update.rate_limited"),
            "the admin is told"
        );
    }

    const TARGET: &str = "x86_64-unknown-linux-musl";

    fn sha(bytes: &[u8]) -> String {
        manifest::sha256_hex(bytes).unwrap()
    }

    /// A signed release at `version` with one registrar build, registered with the fake source.
    fn offer(source: &Fake, key: &minisign::KeyPair, version: &str, build: &[u8]) -> Offer {
        let name = format!("mtpool-registrar-{TARGET}");
        let manifest = format!(
            r#"{{"version":"{version}","artifacts":[{{"component":"mtpool-registrar","target":"{TARGET}","name":"{name}","sha256":"{}"}}]}}"#,
            sha(build)
        );
        let signature = minisign::sign(Some(&key.pk), &key.sk, manifest.as_bytes(), None, None)
            .unwrap()
            .into_string();
        source
            .files
            .lock()
            .unwrap()
            .insert(name.clone(), build.to_vec());
        Offer {
            tag: format!("v{version}"),
            manifest: manifest.into_bytes(),
            signature,
            assets: vec![name, "manifest.json".into(), "manifest.json.minisig".into()],
        }
    }

    thread_local! {
        static KEY: minisign::KeyPair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    }

    fn key() -> minisign::KeyPair {
        KEY.with(|k| minisign::KeyPair {
            pk: k.pk.clone(),
            sk: k.sk.clone(),
        })
    }

    fn files_in(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[tokio::test]
    async fn older_or_same_version_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let source = Fake::new(vec![]);
        let same = offer(&source, &key(), "1.2.3", b"same");
        let older = offer(&source, &key(), "1.2.2", b"older");
        let newer = offer(&source, &key(), "1.2.4", b"newer");
        source.answers.lock().unwrap().extend([
            Ok(Latest::Release(same)),
            Ok(Latest::Release(older)),
            Ok(Latest::Release(newer)),
        ]);
        let (mut u, events) = updater(source.clone(), dir.path());
        let releases = dir.path().join("releases");

        let (o, _) = u.step().await;
        assert_eq!(o, Outcome::Ignored(Version::parse("1.2.3").unwrap()));
        let (o, _) = u.step().await;
        assert_eq!(o, Outcome::Ignored(Version::parse("1.2.2").unwrap()));
        assert!(
            source.downloads.lock().unwrap().is_empty(),
            "nothing is fetched for them"
        );
        assert!(files_in(&releases).is_empty());

        let (o, _) = u.step().await;
        assert_eq!(o, Outcome::Published(Version::parse("1.2.4").unwrap()));
        assert_eq!(
            files_in(&releases),
            [
                "manifest.json",
                "manifest.json.minisig",
                "mtpool-registrar-x86_64-unknown-linux-musl"
            ]
        );
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e.action == "update.published")
        );
    }

    #[tokio::test]
    async fn bad_signature_publishes_nothing_and_is_not_retried() {
        let dir = tempfile::tempdir().unwrap();
        let source = Fake::new(vec![]);
        let stranger = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let bad = offer(&source, &stranger, "9.0.0", b"evil");
        source
            .answers
            .lock()
            .unwrap()
            .extend([Ok(Latest::Release(bad.clone())), Ok(Latest::Release(bad))]);
        let (mut u, events) = updater(source.clone(), dir.path());
        let (o, _) = u.step().await;
        assert!(matches!(o, Outcome::Refused(_)), "{o:?}");
        let (o, _) = u.step().await;
        assert_eq!(o, Outcome::Idle, "the same tag is not looked at again");
        assert!(source.downloads.lock().unwrap().is_empty());
        assert!(files_in(&dir.path().join("releases")).is_empty());
        let events = events.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.action == "update.rejected")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn tampered_download_is_never_published() {
        let dir = tempfile::tempdir().unwrap();
        let source = Fake::new(vec![]);
        let o = offer(&source, &key(), "2.0.0", b"good");
        source
            .files
            .lock()
            .unwrap()
            .insert(format!("mtpool-registrar-{TARGET}"), b"swapped".to_vec());
        source
            .answers
            .lock()
            .unwrap()
            .push_back(Ok(Latest::Release(o)));
        let (mut u, _) = updater(source, dir.path());
        let (o, _) = u.step().await;
        assert!(matches!(o, Outcome::Refused(_)), "{o:?}");
        assert!(
            files_in(&dir.path().join("releases")).is_empty(),
            "not even a staging leftover"
        );
    }

    #[tokio::test]
    async fn a_failed_download_is_tried_again_and_the_manifest_comes_last() {
        let dir = tempfile::tempdir().unwrap();
        let source = Fake::new(vec![]);
        let o = offer(&source, &key(), "2.0.0", b"good");
        let name = format!("mtpool-registrar-{TARGET}");
        let body = source.files.lock().unwrap().remove(&name).unwrap();
        source
            .answers
            .lock()
            .unwrap()
            .push_back(Ok(Latest::Release(o)));
        let (mut u, _) = updater(source.clone(), dir.path());
        let (o, _) = u.step().await;
        assert!(matches!(o, Outcome::Failed(_)), "{o:?}");
        assert!(
            files_in(&dir.path().join("releases")).is_empty(),
            "no manifest without its files"
        );
        source.files.lock().unwrap().insert(name, body);
        // The source answers "unchanged" from now on; the pending release is still retried.
        let (o, _) = u.step().await;
        assert_eq!(o, Outcome::Published(Version::parse("2.0.0").unwrap()));
    }

    #[tokio::test]
    async fn a_rolled_back_release_is_not_published_again() {
        let dir = tempfile::tempdir().unwrap();
        let source = Fake::new(vec![]);
        let o = offer(&source, &key(), "2.0.0", b"good");
        source
            .answers
            .lock()
            .unwrap()
            .push_back(Ok(Latest::Release(o)));
        let (mut u, _) = updater(source.clone(), dir.path());
        u.paths
            .reject(&Version::parse("2.0.0").unwrap(), "rolled back")
            .unwrap();
        let (o, _) = u.step().await;
        assert_eq!(o, Outcome::Ignored(Version::parse("2.0.0").unwrap()));
        assert!(source.downloads.lock().unwrap().is_empty());
    }
}
