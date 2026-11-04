//! Releases for the registrar to hand out: a manifest of agent builds, signed, and the builds
//! themselves, copied into the registrar's releases directory.

use std::path::{Path, PathBuf};

use serde_json::json;
use sha2::{Digest, Sha256};

use crate::signing::TestKey;
use crate::{Node, Result, Stand, StandError, fail, run};

/// Where the registrar of the stand reads releases from (`MTPOOL_RELEASES_DIR` in the compose
/// file).
const RELEASES_DIR: &str = "/srv/releases";

/// What is put into a release.
pub struct ReleaseSpec<'a> {
    pub version: &'a str,
    /// The agent build to offer: a file on this machine.
    pub agent_binary: &'a Path,
    /// The target name the manifest gives the build, such as `aarch64-unknown-linux-musl`.
    pub target: &'a str,
    /// Signs the manifest.
    pub key: &'a TestKey,
}

/// A release that is on the registrar.
#[derive(Debug, Clone)]
pub struct Release {
    pub version: String,
    pub target: String,
    pub artifact: String,
    pub sha256: String,
}

pub(crate) fn scratch_dir(label: &str) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("mtpool-e2e-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&dir)
        .map_err(|e| StandError(format!("cannot create {}: {e}", dir.display())))?;
    Ok(dir)
}

impl Stand {
    /// Puts a release on the registrar: the agent build, a manifest that lists it and the
    /// manifest's signature.
    pub async fn sign_release(&self, spec: &ReleaseSpec<'_>) -> Result<Release> {
        let binary = std::fs::read(spec.agent_binary)
            .map_err(|e| StandError(format!("cannot read {}: {e}", spec.agent_binary.display())))?;
        let sha256: String = Sha256::digest(&binary)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let artifact = format!("mtpool-agent-{}", spec.target);
        let manifest = serde_json::to_vec_pretty(&json!({
            "version": spec.version,
            "artifacts": [{
                "component": "mtpool-agent",
                "target": spec.target,
                "name": artifact,
                "sha256": sha256,
            }],
        }))
        .map_err(|e| StandError(format!("cannot write the manifest: {e}")))?;
        let signature = spec.key.sign(&manifest)?;

        let dir = scratch_dir("release")?;
        let manifest_path = dir.join("manifest.json");
        let signature_path = dir.join("manifest.json.minisig");
        std::fs::write(&manifest_path, &manifest)
            .and_then(|()| std::fs::write(&signature_path, signature))
            .map_err(|e| StandError(format!("cannot write the release files: {e}")))?;
        // The build goes first: a manifest never names a file that is not there yet.
        self.copy_to_registrar(spec.agent_binary, &artifact).await?;
        self.copy_to_registrar(&manifest_path, "manifest.json")
            .await?;
        self.copy_to_registrar(&signature_path, "manifest.json.minisig")
            .await?;
        let _ = std::fs::remove_dir_all(&dir);
        Ok(Release {
            version: spec.version.to_owned(),
            target: spec.target.to_owned(),
            artifact,
            sha256,
        })
    }

    async fn copy_to_registrar(&self, from: &Path, name: &str) -> Result<()> {
        run(
            self.guard
                .compose()
                .arg("cp")
                .arg(from)
                .arg(format!("registrar:{RELEASES_DIR}/{name}")),
            &format!("docker compose cp {name} to the registrar"),
            None,
        )
        .await
        .map(|_| ())
    }
}

impl Node<'_> {
    /// The target name of this node's builds (`uname -m` as a musl triple).
    pub async fn target(&self) -> Result<String> {
        let machine = self.exec("uname -m").await?;
        match machine.trim() {
            "x86_64" => Ok("x86_64-unknown-linux-musl".to_owned()),
            "aarch64" => Ok("aarch64-unknown-linux-musl".to_owned()),
            other => fail(format!("no agent build for {other}")),
        }
    }

    /// Copies the agent of this node's image to this machine, and returns where. A scenario
    /// offers it back as a release: it is a build the node can run.
    pub async fn fetch_agent_binary(&self) -> Result<PathBuf> {
        self.fetch_file("/usr/local/bin/mtpool-agent").await
    }

    /// Copies the file `remote` of this node to this machine, and returns where.
    pub async fn fetch_file(&self, remote: &str) -> Result<PathBuf> {
        let base = remote.rsplit('/').next().unwrap_or("file");
        let path = scratch_dir("agent")?.join(format!("{base}-{}", self.service));
        run(
            self.stand
                .guard
                .compose()
                .arg("cp")
                .arg(format!("{}:{remote}", self.service))
                .arg(&path),
            &format!("docker compose cp of {remote}"),
            None,
        )
        .await?;
        Ok(path)
    }
}
