//! The privileged half: swap the binary, watch the new build come up, put the old one back when
//! it does not. Everything it acts on it checks again from the files; nothing the unprivileged
//! worker said is taken on trust.

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use mtpool_selfupdate::{
    Verdict, previous_of, read_release_file, replace_binary, stage, staged_of, watch,
};

use super::Paths;
use super::manifest::Version;

pub const WATCH_FOR: Duration = Duration::from_secs(60);
pub const PROBE_EVERY: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct ApplyConfig {
    pub paths: Paths,
    /// The installed binary.
    pub bin: PathBuf,
    /// The target the manifest names this host's build by.
    pub target: String,
    /// The signing key of the running build.
    pub key: String,
    /// The version of the running build.
    pub current: Version,
    pub watch: Duration,
    pub probe_every: Duration,
}

pub trait Host {
    fn restart(&self) -> io::Result<()>;
    /// The id of the signing key built into the binary at `path`; `None` when it has none.
    fn binary_key_id(&self, path: &Path) -> Result<Option<String>, String>;
    fn ready(&self) -> impl Future<Output = bool> + Send;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    Updated(Version),
    NothingToDo,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApplyError {
    #[error("release refused: {reason}")]
    Rejected {
        version: Option<Version>,
        reason: String,
    },
    #[error("{0}")]
    Failed(String),
    #[error("version {0} did not become ready in time")]
    WatchFailed(Version),
    #[error("version {0} was rolled back while it was being watched")]
    Superseded(Version),
}

fn rejected(version: Option<&Version>, reason: impl Into<String>) -> ApplyError {
    ApplyError::Rejected {
        version: version.cloned(),
        reason: reason.into(),
    }
}

/// Checks the published release again and, if it is a newer, intact, signed one, installs it:
/// keep the old build, put the new one in place, restart, watch for [`ApplyConfig::watch`].
///
/// Returns `Err(WatchFailed)` when the new build did not come up; the probation marker then stays
/// and the rollback unit (started by this process failing) puts the old build back.
pub async fn apply_self_update<H: Host>(
    cfg: &ApplyConfig,
    host: &H,
) -> Result<Applied, ApplyError> {
    use super::manifest::{self, REGISTRAR_COMPONENT, VerifyError};
    use super::{MANIFEST_NAME, SIGNATURE_NAME};

    let dir = &cfg.paths.releases;
    let bytes = match read_release_file(dir, MANIFEST_NAME, 256 * 1024) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Applied::NothingToDo),
        Err(e) => return Err(ApplyError::Failed(format!("cannot read the manifest: {e}"))),
    };
    let signature = read_release_file(dir, SIGNATURE_NAME, 4096)
        .map_err(|e| rejected(None, format!("cannot read the signature: {e}")))?;
    let signature =
        String::from_utf8(signature).map_err(|_| rejected(None, "the signature is not text"))?;
    let manifest = manifest::verify_manifest_with(&cfg.key, &bytes, &signature)
        .map_err(|e| rejected(None, e.to_string()))?;
    let version = manifest.version.clone();

    let running = match cfg.paths.installed() {
        Some(installed) if installed > cfg.current => installed,
        _ => cfg.current.clone(),
    };
    if version <= running {
        return Ok(Applied::NothingToDo);
    }
    if cfg.paths.is_rejected(&version) {
        tracing::info!(%version, "this release was rolled back before; not applying it again");
        return Ok(Applied::NothingToDo);
    }
    let refuse = |reason: String| {
        if let Err(e) = cfg.paths.reject(&version, &reason) {
            tracing::warn!(error = %e, "cannot record the refused release");
        }
        rejected(Some(&version), reason)
    };

    let artifact = manifest
        .artifact(REGISTRAR_COMPONENT, &cfg.target)
        .ok_or_else(|| {
            refuse(format!(
                "the release has no registrar build for {}",
                cfg.target
            ))
        })?;
    let staged = staged_of(&cfg.bin);
    stage(&dir.join(&artifact.name), &staged, &artifact.sha256).map_err(refuse)?;

    // The key the new build trusts must be the one that signs the next release: the current one,
    // or the one this very release announces.
    let current_id = mtpool_protocol::signing::key_id(&cfg.key);
    let expected_id = match manifest.key_artifact() {
        None => current_id,
        Some(announced) => {
            let file = read_release_file(dir, &announced.name, 4096)
                .map_err(|e| refuse(format!("cannot read the announced key: {e}")));
            let key = file.and_then(|f| {
                manifest::rotated_key(&manifest, &f).map_err(|e: VerifyError| refuse(e.to_string()))
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
    let built_in = match host.binary_key_id(&staged) {
        Ok(id) => id,
        Err(e) => {
            let _ = std::fs::remove_file(&staged);
            return Err(refuse(format!("the new build does not run: {e}")));
        }
    };
    if expected_id.is_none() || built_in != expected_id {
        let _ = std::fs::remove_file(&staged);
        return Err(refuse(
            "the signing key built into the new build is not the one that signs the next release"
                .to_owned(),
        ));
    }

    cfg.paths
        .begin_probation(&version, &running)
        .map_err(|e| ApplyError::Failed(format!("cannot write the probation marker: {e}")))?;
    replace_binary(&cfg.bin, &staged).map_err(|e| {
        let _ = std::fs::remove_file(&staged);
        ApplyError::Failed(format!("cannot replace the binary: {e}"))
    })?;
    tracing::info!(%version, "new build in place, restarting");
    host.restart()
        .map_err(|e| ApplyError::Failed(format!("cannot restart the service: {e}")))?;

    let verdict = watch(cfg.watch, cfg.probe_every, async || host.ready().await).await;
    if verdict == Verdict::Failed {
        return Err(ApplyError::WatchFailed(version));
    }
    // The rollback may have been quicker (the service itself failing starts it too): then the
    // marker is gone and this build is no longer the installed one.
    if cfg.paths.claim_probation().is_none() {
        return Err(ApplyError::Superseded(version));
    }
    cfg.paths
        .set_installed(&version)
        .map_err(|e| ApplyError::Failed(format!("cannot record the installed version: {e}")))?;
    Ok(Applied::Updated(version))
}

/// Puts the previous build back when a new one is on probation. `Ok(None)` when none is: nothing
/// to roll back, nothing changed. The rolled-back version is remembered so that it is not
/// installed again.
pub async fn rollback<H: Host>(cfg: &ApplyConfig, host: &H) -> Result<Option<Version>, ApplyError> {
    if cfg.paths.probation().is_none() {
        return Ok(None);
    }
    let prev = previous_of(&cfg.bin);
    if !prev.is_file() {
        return Err(ApplyError::Failed(format!(
            "there is no previous build at {}",
            prev.display()
        )));
    }
    // One of the callers gets the marker; the other has nothing to do.
    let Some(marker) = cfg.paths.claim_probation() else {
        return Ok(None);
    };
    std::fs::rename(&prev, &cfg.bin)
        .map_err(|e| ApplyError::Failed(format!("cannot restore the previous build: {e}")))?;
    if let Err(e) = cfg
        .paths
        .reject(&marker.version, "did not become ready after the update")
    {
        tracing::warn!(error = %e, "cannot record the rolled-back release");
    }
    host.restart()
        .map_err(|e| ApplyError::Failed(format!("cannot restart the service: {e}")))?;
    tracing::warn!(version = %marker.version, back_to = %marker.previous, "rolled back");
    Ok(Some(marker.version))
}

/// The real machine: systemd, the installed binary and the registrar's own port.
pub struct SystemHost {
    /// Where the registrar listens; `/readyz` is asked there.
    pub listen: std::net::SocketAddr,
    pub unit: String,
}

impl SystemHost {
    fn probe_addr(&self) -> std::net::SocketAddr {
        let mut addr = self.listen;
        if addr.ip().is_unspecified() {
            addr.set_ip(if addr.is_ipv4() {
                std::net::Ipv4Addr::LOCALHOST.into()
            } else {
                std::net::Ipv6Addr::LOCALHOST.into()
            });
        }
        addr
    }
}

impl Host for SystemHost {
    fn restart(&self) -> io::Result<()> {
        use std::process::Command;
        // A unit that hit its start limit has to be cleared before it will start again.
        let _ = Command::new("systemctl")
            .args(["reset-failed", &self.unit])
            .status();
        let status = Command::new("systemctl")
            .args(["restart", &self.unit])
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "systemctl restart {} failed: {status}",
                self.unit
            )))
        }
    }

    fn binary_key_id(&self, path: &Path) -> Result<Option<String>, String> {
        mtpool_selfupdate::binary_key_id(path)
    }

    async fn ready(&self) -> bool {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let ask = async {
            let mut stream = tokio::net::TcpStream::connect(self.probe_addr())
                .await
                .ok()?;
            stream
                .write_all(b"GET /readyz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .await
                .ok()?;
            let mut head = [0u8; 32];
            let mut got = 0;
            while got < head.len() {
                match stream.read(&mut head[got..]).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => got += n,
                }
            }
            Some(head[..got].starts_with(b"HTTP/1.1 200"))
        };
        matches!(
            tokio::time::timeout(Duration::from_secs(5), ask).await,
            Ok(Some(true))
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use sha2::{Digest, Sha256};

    use super::*;

    const TARGET: &str = "x86_64-unknown-linux-musl";

    fn hex(bytes: &[u8]) -> String {
        data_encoding::HEXLOWER.encode(&Sha256::digest(bytes))
    }

    fn pair() -> minisign::KeyPair {
        minisign::KeyPair::generate_unencrypted_keypair().unwrap()
    }

    fn sign(pair: &minisign::KeyPair, data: &[u8]) -> String {
        minisign::sign(Some(&pair.pk), &pair.sk, data, None, None)
            .unwrap()
            .into_string()
    }

    struct FakeHost {
        restarts: AtomicUsize,
        key_id: Mutex<Option<String>>,
        ready: bool,
    }

    impl FakeHost {
        fn ready(key_id: &str) -> FakeHost {
            FakeHost {
                restarts: AtomicUsize::new(0),
                key_id: Mutex::new(Some(key_id.to_owned())),
                ready: true,
            }
        }
    }

    impl Host for FakeHost {
        fn restart(&self) -> io::Result<()> {
            self.restarts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn binary_key_id(&self, _path: &Path) -> Result<Option<String>, String> {
            Ok(self.key_id.lock().unwrap().clone())
        }
        async fn ready(&self) -> bool {
            self.ready
        }
    }

    struct Rig {
        _dir: tempfile::TempDir,
        cfg: ApplyConfig,
        key: minisign::KeyPair,
        key_id: String,
    }

    fn rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            releases: dir.path().join("releases"),
            state: dir.path().join("state"),
        };
        std::fs::create_dir_all(&paths.releases).unwrap();
        std::fs::create_dir_all(&paths.state).unwrap();
        let bin = dir.path().join("mtpool-registrar");
        std::fs::write(&bin, b"old build").unwrap();
        let key = pair();
        let key_id = mtpool_protocol::signing::key_id(&key.pk.to_base64()).unwrap();
        let cfg = ApplyConfig {
            paths,
            bin,
            target: TARGET.to_owned(),
            key: key.pk.to_base64(),
            current: Version::parse("1.0.0").unwrap(),
            watch: Duration::from_secs(60),
            probe_every: Duration::from_secs(2),
        };
        Rig {
            _dir: dir,
            cfg,
            key,
            key_id,
        }
    }

    impl Rig {
        /// Publishes a release into the releases directory the way the worker does.
        fn publish(&self, version: &str, build: &[u8]) {
            self.publish_signed_by(&self.key, version, build);
        }

        fn publish_signed_by(&self, signer: &minisign::KeyPair, version: &str, build: &[u8]) {
            let name = format!("mtpool-registrar-{TARGET}");
            let manifest = format!(
                r#"{{"version":"{version}","artifacts":[{{"component":"mtpool-registrar","target":"{TARGET}","name":"{name}","sha256":"{}"}}]}}"#,
                hex(build)
            );
            let dir = &self.cfg.paths.releases;
            std::fs::write(dir.join(&name), build).unwrap();
            std::fs::write(
                dir.join("manifest.json.minisig"),
                sign(signer, manifest.as_bytes()),
            )
            .unwrap();
            std::fs::write(dir.join("manifest.json"), manifest).unwrap();
        }

        fn bin(&self) -> Vec<u8> {
            std::fs::read(&self.cfg.bin).unwrap()
        }

        fn prev(&self) -> PathBuf {
            let mut name = self.cfg.bin.clone().into_os_string();
            name.push(".prev");
            PathBuf::from(name)
        }
    }

    #[test]
    fn atomic_replace_keeps_previous() {
        let rig = rig();
        let staged = rig.cfg.bin.with_extension("new");
        std::fs::write(&staged, b"new build").unwrap();
        replace_binary(&rig.cfg.bin, &staged).unwrap();
        assert_eq!(rig.bin(), b"new build");
        assert_eq!(std::fs::read(rig.prev()).unwrap(), b"old build");
        assert!(
            !staged.exists(),
            "the staged file is moved, not left behind"
        );

        // A second replacement keeps the build that was running, not the first one.
        std::fs::write(&staged, b"newer build").unwrap();
        replace_binary(&rig.cfg.bin, &staged).unwrap();
        assert_eq!(rig.bin(), b"newer build");
        assert_eq!(std::fs::read(rig.prev()).unwrap(), b"new build");
    }

    #[tokio::test(start_paused = true)]
    async fn watch_passes_when_ready_and_fails_without_it() {
        let ready = watch(WATCH_FOR, PROBE_EVERY, async || true).await;
        assert_eq!(ready, Verdict::Ready);
        let started = tokio::time::Instant::now();
        let never = watch(WATCH_FOR, PROBE_EVERY, async || false).await;
        assert_eq!(never, Verdict::Failed);
        assert!(started.elapsed() >= WATCH_FOR, "it waits the whole window");
        assert!(started.elapsed() < WATCH_FOR + Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn watch_fails_when_the_build_stops_answering() {
        let mut answers = vec![false, false, true, false].into_iter();
        let verdict = watch(WATCH_FOR, PROBE_EVERY, async || {
            answers.next().unwrap_or(false)
        })
        .await;
        assert_eq!(verdict, Verdict::Failed, "ready once, then down: no pass");
    }

    #[tokio::test(start_paused = true)]
    async fn apply_switches_and_confirms() {
        let rig = rig();
        rig.publish("2.0.0", b"new build");
        let host = FakeHost::ready(&rig.key_id);
        let applied = apply_self_update(&rig.cfg, &host).await.unwrap();
        assert_eq!(applied, Applied::Updated(Version::parse("2.0.0").unwrap()));
        assert_eq!(rig.bin(), b"new build");
        assert_eq!(std::fs::read(rig.prev()).unwrap(), b"old build");
        assert_eq!(host.restarts.load(Ordering::SeqCst), 1);
        assert_eq!(
            rig.cfg.paths.installed(),
            Some(Version::parse("2.0.0").unwrap())
        );
        assert!(rig.cfg.paths.probation().is_none());
        // Nothing new: nothing to do, and no restart.
        let again = apply_self_update(
            &ApplyConfig {
                current: Version::parse("2.0.0").unwrap(),
                ..rig.cfg.clone()
            },
            &host,
        )
        .await;
        assert_eq!(again, Ok(Applied::NothingToDo));
        assert_eq!(host.restarts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failed_watch_is_rolled_back_and_not_tried_again() {
        let rig = rig();
        rig.publish("2.0.0", b"broken build");
        let host = FakeHost {
            ready: false,
            ..FakeHost::ready(&rig.key_id)
        };
        let cfg = ApplyConfig {
            watch: Duration::from_millis(30),
            probe_every: Duration::from_millis(10),
            ..rig.cfg.clone()
        };
        let err = apply_self_update(&cfg, &host).await.unwrap_err();
        assert_eq!(
            err,
            ApplyError::WatchFailed(Version::parse("2.0.0").unwrap())
        );
        assert_eq!(
            rig.bin(),
            b"broken build",
            "the switch back is the rollback unit's job"
        );
        assert!(cfg.paths.probation().is_some());
        assert_eq!(cfg.paths.installed(), None);

        let back = rollback(&cfg, &host).await.unwrap();
        assert_eq!(back, Some(Version::parse("2.0.0").unwrap()));
        assert_eq!(rig.bin(), b"old build");
        assert!(cfg.paths.probation().is_none());
        assert!(cfg.paths.is_rejected(&Version::parse("2.0.0").unwrap()));

        // The same release is offered again: it is refused, the binary stays.
        let again = apply_self_update(&cfg, &host).await;
        assert!(
            matches!(
                again,
                Ok(Applied::NothingToDo) | Err(ApplyError::Rejected { .. })
            ),
            "{again:?}"
        );
        assert_eq!(rig.bin(), b"old build");
        // A second rollback with nothing in probation changes nothing.
        assert_eq!(rollback(&cfg, &host).await.unwrap(), None);
        assert_eq!(rig.bin(), b"old build");
    }

    #[tokio::test]
    async fn bad_signature_changes_nothing() {
        let rig = rig();
        rig.publish_signed_by(&pair(), "2.0.0", b"new build");
        let host = FakeHost::ready(&rig.key_id);
        let err = apply_self_update(&rig.cfg, &host).await.unwrap_err();
        assert!(
            matches!(err, ApplyError::Rejected { version: None, .. }),
            "{err:?}"
        );
        assert_eq!(rig.bin(), b"old build");
        assert!(!rig.prev().exists());
        assert_eq!(host.restarts.load(Ordering::SeqCst), 0);
        assert!(rig.cfg.paths.probation().is_none());
    }

    #[tokio::test]
    async fn tampered_artifact_changes_nothing() {
        let rig = rig();
        rig.publish("2.0.0", b"new build");
        std::fs::write(
            rig.cfg
                .paths
                .releases
                .join(format!("mtpool-registrar-{TARGET}")),
            b"new build with something added",
        )
        .unwrap();
        let host = FakeHost::ready(&rig.key_id);
        let err = apply_self_update(&rig.cfg, &host).await.unwrap_err();
        assert!(matches!(err, ApplyError::Rejected { .. }), "{err:?}");
        assert_eq!(rig.bin(), b"old build");
        assert_eq!(host.restarts.load(Ordering::SeqCst), 0);
        assert!(
            !rig.cfg.bin.with_extension("new").exists(),
            "no staged file stays"
        );
    }

    #[tokio::test]
    async fn unannounced_key_change_is_refused() {
        let rig = rig();
        rig.publish("2.0.0", b"new build");
        let host = FakeHost::ready("0123456789ABCDEF");
        let err = apply_self_update(&rig.cfg, &host).await.unwrap_err();
        assert!(matches!(err, ApplyError::Rejected { .. }), "{err:?}");
        assert_eq!(rig.bin(), b"old build");
        // A build with no key at all cannot verify the next release either.
        *host.key_id.lock().unwrap() = None;
        rig.publish("2.0.1", b"newer build");
        let err = apply_self_update(&rig.cfg, &host).await.unwrap_err();
        assert!(matches!(err, ApplyError::Rejected { .. }), "{err:?}");
        assert_eq!(host.restarts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn older_release_is_not_applied() {
        let rig = rig();
        rig.publish("0.9.0", b"older build");
        let host = FakeHost::ready(&rig.key_id);
        assert_eq!(
            apply_self_update(&rig.cfg, &host).await,
            Ok(Applied::NothingToDo)
        );
        assert_eq!(rig.bin(), b"old build");
    }
}
