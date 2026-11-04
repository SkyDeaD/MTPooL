use std::collections::HashMap;
use std::sync::Mutex;

use super::*;
use crate::host::testing::ScriptedRunner;

const TARGET: &str = "x86_64-unknown-linux-musl";

fn key_pair() -> minisign::KeyPair {
    minisign::KeyPair::generate_unencrypted_keypair().unwrap()
}

fn key_id(pair: &minisign::KeyPair) -> String {
    mtpool_protocol::signing::key_id(&pair.pk.to_base64()).unwrap()
}

/// A build that answers `version` the way the agent does.
fn build(version: &str, key_id: &str) -> Vec<u8> {
    format!("#!/bin/sh\necho '{version} (signing key {key_id})'\n").into_bytes()
}

#[derive(Default)]
struct FakeSource {
    offer: Mutex<Option<String>>,
    files: Mutex<HashMap<String, Vec<u8>>>,
    fetched: Mutex<Vec<String>>,
    reports: Mutex<Vec<UpdateStatus>>,
    accept_reports: Mutex<bool>,
}

impl FakeSource {
    fn publish(&self, signer: &minisign::KeyPair, version: &str, artifact: &[u8], sha: &str) {
        let name = format!("mtpool-agent-{TARGET}");
        let manifest = format!(
            r#"{{"version":"{version}","artifacts":[{{"component":"mtpool-agent","target":"{TARGET}","name":"{name}","sha256":"{sha}"}}]}}"#
        )
        .into_bytes();
        let signature = minisign::sign(Some(&signer.pk), &signer.sk, &manifest[..], None, None)
            .unwrap()
            .into_string();
        let mut files = self.files.lock().unwrap();
        files.insert("manifest.json".into(), manifest);
        files.insert("manifest.json.minisig".into(), signature.into_bytes());
        files.insert(name, artifact.to_vec());
    }
    fn reports(&self) -> Vec<UpdateStatus> {
        self.reports.lock().unwrap().clone()
    }
}

impl Source for &FakeSource {
    async fn offer(&self, _running: &Version) -> Result<Option<String>, String> {
        Ok(self.offer.lock().unwrap().clone())
    }
    async fn fetch(&self, name: &str, dest: &Path, _max: u64) -> Result<(), String> {
        self.fetched.lock().unwrap().push(name.to_owned());
        let body = self.files.lock().unwrap().get(name).cloned();
        match body {
            Some(body) => std::fs::write(dest, body).map_err(|e| e.to_string()),
            None => Err("404".to_owned()),
        }
    }
    async fn report(&self, status: &UpdateStatus) -> bool {
        self.reports.lock().unwrap().push(status.clone());
        *self.accept_reports.lock().unwrap()
    }
}

struct Rig {
    dir: tempfile::TempDir,
    cfg: UpdaterConfig,
    trusted: minisign::KeyPair,
    source: FakeSource,
    runner: std::sync::Arc<ScriptedRunner>,
}

struct SharedRunner(std::sync::Arc<ScriptedRunner>);

impl CommandRunner for SharedRunner {
    async fn run(
        &self,
        program: &str,
        args: &[&str],
        stdin: Option<&[u8]>,
    ) -> io::Result<std::process::Output> {
        self.0.run(program, args, stdin).await
    }
}

fn old_build(trusted: &minisign::KeyPair) -> Vec<u8> {
    build("0.1.0", &key_id(trusted))
}

impl Rig {
    fn new() -> Rig {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let trusted = key_pair();
        let bin = dir.path().join("mtpool-agent");
        std::fs::write(&bin, old_build(&trusted)).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let state = dir.path().join("state");
        let releases = dir.path().join("download");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&releases).unwrap();
        let cfg = UpdaterConfig {
            paths: Paths { releases, state },
            bin,
            target: TARGET.to_owned(),
            key: trusted.pk.to_base64(),
            current: Version::parse("0.1.0").unwrap(),
            watch: Duration::from_secs(20),
            probe_every: Duration::from_secs(1),
            retry_after: Duration::from_secs(300),
        };
        let source = FakeSource::default();
        *source.accept_reports.lock().unwrap() = true;
        Rig {
            dir,
            cfg,
            trusted,
            source,
            runner: std::sync::Arc::new(ScriptedRunner::new()),
        }
    }

    /// Two good answers for the restart: reset-failed and restart.
    fn script_restart(&self) {
        self.runner.push_output(0, b"");
        self.runner.push_output(0, b"");
    }

    fn updater(&self) -> AgentUpdater<&FakeSource, SystemHost<SharedRunner>> {
        AgentUpdater::new(
            self.cfg.clone(),
            &self.source,
            SystemHost {
                runner: SharedRunner(self.runner.clone()),
                unit: "mtpool-agent".to_owned(),
            },
        )
    }

    /// The updater of the process that runs `version` (after a restart).
    fn updater_running(
        &self,
        version: &str,
    ) -> AgentUpdater<&FakeSource, SystemHost<SharedRunner>> {
        let mut cfg = self.cfg.clone();
        cfg.current = Version::parse(version).unwrap();
        AgentUpdater::new(
            cfg,
            &self.source,
            SystemHost {
                runner: SharedRunner(self.runner.clone()),
                unit: "mtpool-agent".to_owned(),
            },
        )
    }

    fn offer(&self, version: &str) {
        *self.source.offer.lock().unwrap() = Some(version.to_owned());
    }

    fn publish_good(&self, version: &str) -> Vec<u8> {
        let artifact = build(version, &key_id(&self.trusted));
        let sha = mtpool_selfupdate::manifest::sha256_hex(&artifact[..]).unwrap();
        self.source.publish(&self.trusted, version, &artifact, &sha);
        artifact
    }

    fn installed_bytes(&self) -> Vec<u8> {
        std::fs::read(&self.cfg.bin).unwrap()
    }
}

#[tokio::test]
async fn agent_rejects_bad_signature() {
    let rig = Rig::new();
    let stranger = key_pair();
    let artifact = build("0.2.0", &key_id(&rig.trusted));
    let sha = mtpool_selfupdate::manifest::sha256_hex(&artifact[..]).unwrap();
    rig.source.publish(&stranger, "0.2.0", &artifact, &sha);
    rig.offer("0.2.0");

    let got = rig.updater().check().await;

    assert_eq!(got, Checked::Failed("bad_signature".to_owned()));
    assert_eq!(
        rig.installed_bytes(),
        old_build(&rig.trusted),
        "binary untouched"
    );
    assert!(rig.cfg.paths.probation().is_none());
    assert!(rig.runner.calls().is_empty(), "nothing restarted");
    let reports = rig.source.reports();
    let last = reports.last().expect("the failure is reported");
    assert_eq!(last.outcome, UpdateOutcome::Failed);
    assert_eq!(last.code.as_deref(), Some("bad_signature"));
    // The build was never even downloaded.
    assert!(
        !rig.source
            .fetched
            .lock()
            .unwrap()
            .iter()
            .any(|n| n.starts_with("mtpool-agent-"))
    );
}

#[tokio::test]
async fn agent_update_does_not_restart_engine() {
    let rig = Rig::new();
    let artifact = rig.publish_good("0.2.0");
    rig.offer("0.2.0");
    rig.script_restart();

    let got = rig.updater().check().await;

    assert_eq!(got, Checked::Restarting(Version::parse("0.2.0").unwrap()));
    assert_eq!(rig.installed_bytes(), artifact, "the new build is in place");
    assert_eq!(
        std::fs::read(mtpool_selfupdate::previous_of(&rig.cfg.bin)).unwrap(),
        old_build(&rig.trusted),
        "the old build is kept beside it"
    );
    let marker = rig.cfg.paths.probation().expect("probation marker");
    assert_eq!(marker.version, Version::parse("0.2.0").unwrap());
    assert_eq!(marker.previous, Version::parse("0.1.0").unwrap());
    // Every command is about the agent's own unit; no engine is named or touched.
    let calls = rig.runner.calls();
    assert!(!calls.is_empty());
    for call in &calls {
        assert_eq!(call.program, "systemctl", "{call:?}");
        assert_eq!(
            call.args.last().map(String::as_str),
            Some("mtpool-agent"),
            "{call:?}"
        );
        assert!(
            call.args
                .iter()
                .all(|a| !a.contains("telemt") && !a.contains("mtproxy"))
        );
    }
    assert!(
        calls
            .iter()
            .any(|c| c.args.first().map(String::as_str) == Some("restart"))
    );
}

#[tokio::test(start_paused = true)]
async fn agent_rollback_when_start_fails() {
    let rig = Rig::new();
    rig.publish_good("0.2.0");
    rig.offer("0.2.0");
    rig.script_restart();
    assert!(matches!(
        rig.updater().check().await,
        Checked::Restarting(_)
    ));

    // The new build never gets healthy.
    rig.script_restart();
    let got = rig
        .updater_running("0.2.0")
        .confirm(|| async { false })
        .await;

    assert_eq!(got, Confirmed::RolledBack(Version::parse("0.2.0").unwrap()));
    assert_eq!(
        rig.installed_bytes(),
        old_build(&rig.trusted),
        "the old build is back"
    );
    assert!(rig.cfg.paths.probation().is_none());
    assert!(rig.cfg.paths.is_rejected(&Version::parse("0.2.0").unwrap()));
    // The old build tells the registrar once it is up.
    *rig.source.accept_reports.lock().unwrap() = true;
    rig.updater().flush_outbox().await;
    let last = rig.source.reports().pop().expect("a report");
    assert_eq!(last.outcome, UpdateOutcome::RolledBack);
    assert_eq!(last.version, "0.2.0");
    // A rolled-back release is not installed again.
    rig.source.fetched.lock().unwrap().clear();
    let again = rig.updater();
    assert_eq!(
        again.check().await,
        Checked::Failed("rejected_before".to_owned())
    );
    assert_eq!(
        again.check().await,
        Checked::Idle,
        "told once, not every poll"
    );
    assert!(rig.source.fetched.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_healthy_new_build_is_confirmed_and_reported() {
    let rig = Rig::new();
    rig.publish_good("0.2.0");
    rig.offer("0.2.0");
    rig.script_restart();
    rig.updater().check().await;

    let new_process = rig.updater_running("0.2.0");
    let got = new_process.confirm(|| async { true }).await;

    assert_eq!(got, Confirmed::Updated(Version::parse("0.2.0").unwrap()));
    assert!(rig.cfg.paths.probation().is_none());
    assert_eq!(rig.cfg.paths.installed(), Version::parse("0.2.0"));
    let last = rig.source.reports().pop().unwrap();
    assert_eq!(last.outcome, UpdateOutcome::Updated);
    assert_eq!(last.version, "0.2.0");
}

#[tokio::test]
async fn a_report_the_registrar_did_not_take_is_sent_again() {
    let rig = Rig::new();
    let artifact = build("0.2.0", &key_id(&rig.trusted));
    let stranger = key_pair();
    rig.source
        .publish(&stranger, "0.2.0", &artifact, &"ab".repeat(32));
    rig.offer("0.2.0");
    *rig.source.accept_reports.lock().unwrap() = false;

    rig.updater().check().await;
    let first = rig.source.reports().len();
    rig.updater().flush_outbox().await;
    assert!(
        rig.source.reports().len() > first,
        "sent again while not taken"
    );

    *rig.source.accept_reports.lock().unwrap() = true;
    rig.updater().flush_outbox().await;
    let sent = rig.source.reports().len();
    rig.updater().flush_outbox().await;
    assert_eq!(rig.source.reports().len(), sent, "nothing left to send");
}

#[tokio::test]
async fn an_older_release_is_never_installed() {
    let rig = Rig::new();
    // The registrar offers 0.0.9, and the manifest it serves is older too.
    rig.publish_good("0.0.9");
    rig.offer("0.0.9");
    assert_eq!(rig.updater().check().await, Checked::Idle);
    assert!(rig.runner.calls().is_empty());

    // The registrar claims 0.2.0 but serves the old signed manifest: replay.
    rig.offer("0.2.0");
    assert_eq!(
        rig.updater().check().await,
        Checked::Failed("version_mismatch".to_owned())
    );
    assert_eq!(rig.installed_bytes(), old_build(&rig.trusted));
    assert!(rig.cfg.paths.probation().is_none());
}

#[tokio::test]
async fn a_build_with_the_wrong_checksum_is_refused_and_remembered() {
    let rig = Rig::new();
    let artifact = build("0.2.0", &key_id(&rig.trusted));
    rig.source
        .publish(&rig.trusted, "0.2.0", &artifact, &"ab".repeat(32));
    rig.offer("0.2.0");

    assert_eq!(
        rig.updater().check().await,
        Checked::Failed("bad_checksum".to_owned())
    );
    assert_eq!(rig.installed_bytes(), old_build(&rig.trusted));
    assert!(!mtpool_selfupdate::staged_of(&rig.cfg.bin).exists());
    assert!(rig.cfg.paths.is_rejected(&Version::parse("0.2.0").unwrap()));
}

#[tokio::test]
async fn a_build_with_another_signing_key_inside_is_refused() {
    let rig = Rig::new();
    let artifact = build("0.2.0", "0000000000000000");
    let sha = mtpool_selfupdate::manifest::sha256_hex(&artifact[..]).unwrap();
    rig.source.publish(&rig.trusted, "0.2.0", &artifact, &sha);
    rig.offer("0.2.0");

    assert_eq!(
        rig.updater().check().await,
        Checked::Failed("key_mismatch".to_owned())
    );
    assert_eq!(rig.installed_bytes(), old_build(&rig.trusted));
    assert!(rig.cfg.paths.probation().is_none());
}

#[tokio::test]
async fn nothing_offered_means_nothing_happens() {
    let rig = Rig::new();
    assert_eq!(rig.updater().check().await, Checked::Idle);
    assert!(rig.source.reports().is_empty());
    assert!(rig.runner.calls().is_empty());
    let _ = &rig.dir;
}
