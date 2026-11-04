//! The registrar's self-update on the stand: a release of the registrar put on the GitHub
//! stand-in (`mtpool-fakes`), and a fresh machine told to look there instead of at GitHub.

use std::path::PathBuf;

use serde_json::json;
use sha2::{Digest, Sha256};

use crate::signing::TestKey;
use crate::{Result, Stand, StandError, fail, run};

/// The files of one GitHub release of the registrar: `(name, bytes)`, the manifest last.
pub struct RegistrarRelease {
    pub version: String,
    pub files: Vec<(String, Vec<u8>)>,
}

/// A registrar release: `binary` as the build for `target`, and a manifest signed by `key`.
pub fn registrar_release(
    version: &str,
    target: &str,
    binary: &[u8],
    key: &TestKey,
) -> Result<RegistrarRelease> {
    let sha256: String = Sha256::digest(binary)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let name = format!("mtpool-registrar-{target}");
    let manifest = serde_json::to_vec_pretty(&json!({
        "version": version,
        "artifacts": [{
            "component": "mtpool-registrar",
            "target": target,
            "name": name,
            "sha256": sha256,
        }],
    }))
    .map_err(|e| StandError(format!("cannot write the manifest: {e}")))?;
    let signature = key.sign(&manifest)?;
    Ok(RegistrarRelease {
        version: version.to_owned(),
        files: vec![
            (name, binary.to_vec()),
            ("manifest.json".to_owned(), manifest),
            ("manifest.json.minisig".to_owned(), signature.into_bytes()),
        ],
    })
}

impl Stand {
    /// The registrar build of the stand's image, copied to this machine.
    pub async fn fetch_registrar_binary(&self) -> Result<PathBuf> {
        let path = crate::release::scratch_dir("registrar-build")?.join("mtpool-registrar");
        run(
            self.guard
                .compose()
                .arg("cp")
                .arg("registrar:/usr/local/bin/mtpool-registrar")
                .arg(&path),
            "docker compose cp of the registrar build",
            None,
        )
        .await?;
        Ok(path)
    }

    /// Uploads the files of `release` to the GitHub stand-in and publishes them as the newest
    /// release, tagged `v<version>`.
    pub async fn publish_github_release(&self, release: &RegistrarRelease) -> Result<()> {
        for (name, bytes) in &release.files {
            let response = self
                .http
                .put(format!("{}/__github/files/{name}", self.fakes_url))
                .body(bytes.clone())
                .send()
                .await
                .map_err(|e| StandError(format!("upload of {name}: {}", e.without_url())))?;
            if !response.status().is_success() {
                return fail(format!("upload of {name}: {}", response.status()));
            }
        }
        let response = self
            .http
            .post(format!(
                "{}/__github/publish?tag=v{}",
                self.fakes_url, release.version
            ))
            .send()
            .await
            .map_err(|e| StandError(format!("publish: {}", e.without_url())))?;
        if !response.status().is_success() {
            return fail(format!("publish: {}", response.status()));
        }
        Ok(())
    }

    /// The files the GitHub stand-in has served, in order.
    pub async fn github_downloads(&self) -> Result<Vec<String>> {
        let state = self.fakes_state().await?;
        Ok(state["github"]["downloads"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Points the registrar of the fresh machine at the GitHub stand-in and polls every two
    /// seconds, and restarts the service (the installer has started it) so that it reads the new
    /// environment.
    pub async fn point_fresh_registrar_at_fakes(&self) -> Result<()> {
        let run = self
            .fresh_exec(
                "set -e; f=/etc/mtpool/registrar.env; \
                 grep -v '^MTPOOL_RELEASE_REPO=' $f > $f.new; \
                 printf '%s\\n' MTPOOL_RELEASE_REPO=stand/mtpool \
                   MTPOOL_GITHUB_URL=http://fakes:9000/github \
                   MTPOOL_GITHUB_DOWNLOAD_URL=http://fakes:9000/github/dl \
                   MTPOOL_UPDATE_POLL_SECS=2 >> $f.new; \
                 chown --reference=$f $f.new; chmod --reference=$f $f.new; mv $f.new $f; \
                 systemctl restart mtpool-registrar.service",
            )
            .await?;
        if !run.success {
            return fail(format!(
                "cannot edit the registrar environment: {}",
                run.output
            ));
        }
        Ok(())
    }

    /// The target name of the fresh machine's builds.
    pub async fn fresh_target(&self) -> Result<String> {
        let run = self.fresh_exec("uname -m").await?;
        match run.output.trim() {
            "x86_64" => Ok("x86_64-unknown-linux-musl".to_owned()),
            "aarch64" => Ok("aarch64-unknown-linux-musl".to_owned()),
            other => fail(format!("no registrar build for {other}")),
        }
    }
}
