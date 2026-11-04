//! The signing key of the stand. Its public half is built into the registrar and the agent of
//! the images (and so into the installer the registrar serves); its secret half signs the
//! releases the scenarios publish. The key is for tests only and lives in `e2e/keys/`, which git
//! ignores: it is made on first use and kept, so the images stay the same between runs.

use std::path::PathBuf;

use minisign::{KeyPair, PublicKey, SecretKey, SecretKeyBox};

use crate::{Result, StandError, fail};

/// A minisign key pair without a password.
pub struct TestKey {
    secret: SecretKey,
    public_line: String,
}

impl TestKey {
    /// A new key no image knows: what a release signed by somebody else looks like.
    pub fn generate() -> Result<TestKey> {
        let pair = KeyPair::generate_unencrypted_keypair()
            .map_err(|e| StandError(format!("cannot generate a key: {e}")))?;
        Ok(TestKey {
            public_line: pair.pk.to_base64(),
            secret: pair.sk,
        })
    }

    /// The public key as it is written in the installer: the base64 line.
    pub fn public(&self) -> &str {
        &self.public_line
    }

    /// The minisign signature file (`.minisig`) for `data`.
    pub fn sign(&self, data: &[u8]) -> Result<String> {
        let signature = minisign::sign(None, &self.secret, data, None, None)
            .map_err(|e| StandError(format!("cannot sign: {e}")))?;
        Ok(signature.into_string())
    }
}

fn key_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../e2e/keys")
}

/// The stand's key, made if there is none yet. Safe to call from several tests at once.
pub fn ensure_test_key() -> Result<TestKey> {
    let dir = key_dir();
    let public_path = dir.join("minisign.pub");
    let secret_path = dir.join("minisign.key");
    if !(public_path.exists() && secret_path.exists()) {
        std::fs::create_dir_all(&dir)
            .map_err(|e| StandError(format!("cannot create {}: {e}", dir.display())))?;
        let pair = KeyPair::generate_unencrypted_keypair()
            .map_err(|e| StandError(format!("cannot generate a key: {e}")))?;
        let public_box = pair
            .pk
            .to_box()
            .map_err(|e| StandError(format!("cannot write the public key: {e}")))?;
        let secret_box = pair
            .sk
            .to_box(None)
            .map_err(|e| StandError(format!("cannot write the secret key: {e}")))?;
        // Written under a name of our own and moved into place, so a second process never reads
        // half a file. The secret goes first: the public half is what says the pair exists.
        let tag = std::process::id();
        let secret_tmp = dir.join(format!("minisign.key.{tag}"));
        let public_tmp = dir.join(format!("minisign.pub.{tag}"));
        std::fs::write(&secret_tmp, secret_box.into_string())
            .and_then(|()| std::fs::write(&public_tmp, public_box.into_string()))
            .and_then(|()| std::fs::rename(&secret_tmp, &secret_path))
            .and_then(|()| std::fs::rename(&public_tmp, &public_path))
            .map_err(|e| StandError(format!("cannot write the key files: {e}")))?;
    }
    let public_text = std::fs::read_to_string(&public_path)
        .map_err(|e| StandError(format!("cannot read {}: {e}", public_path.display())))?;
    let secret_text = std::fs::read_to_string(&secret_path)
        .map_err(|e| StandError(format!("cannot read {}: {e}", secret_path.display())))?;
    let public = PublicKey::from_box(
        minisign::PublicKeyBox::from_string(&public_text)
            .map_err(|e| StandError(format!("{}: {e}", public_path.display())))?,
    )
    .map_err(|e| StandError(format!("{}: {e}", public_path.display())))?;
    let secret = SecretKeyBox::from_string(&secret_text)
        .and_then(SecretKeyBox::into_unencrypted_secret_key)
        .map_err(|e| StandError(format!("{}: {e}", secret_path.display())))?;
    if secret.keynum() != public.keynum() {
        return fail(format!(
            "{} and {} are not a pair; remove both to make a new one",
            public_path.display(),
            secret_path.display()
        ));
    }
    Ok(TestKey {
        public_line: public.to_base64(),
        secret,
    })
}
