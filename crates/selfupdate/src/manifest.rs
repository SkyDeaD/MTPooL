//! The release manifest and the checks that come before anything in it is trusted.
//!
//! The manifest is `{version, artifacts: [{component, target, name, sha256}]}`. Nothing in it is
//! looked at until the minisign signature over its exact bytes has been verified.

use std::io::Read;
use std::path::Path;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{MANIFEST_NAME, SIGNATURE_NAME};

/// The component of a build of the registrar.
pub const REGISTRAR_COMPONENT: &str = "mtpool-registrar";
/// The component of a build of the node agent.
pub const AGENT_COMPONENT: &str = "mtpool-agent";
/// The component of a new signing key, announced by a release that the old key signed.
pub const KEY_COMPONENT: &str = "mtpool-signing-key";
pub const MAX_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_ARTIFACTS: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("no signing key is built into this binary")]
    NoKey,
    #[error("the signing key is not a minisign public key")]
    BadKey,
    #[error("the signature does not match")]
    BadSignature,
    #[error("the manifest is malformed: {0}")]
    Malformed(String),
    #[error("the checksum does not match")]
    ChecksumMismatch,
    #[error("cannot read the file: {0}")]
    Io(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Pre {
    Num(u64),
    Alpha(String),
}

impl PartialOrd for Pre {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Pre {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (Pre::Num(a), Pre::Num(b)) => a.cmp(b),
            (Pre::Num(_), Pre::Alpha(_)) => std::cmp::Ordering::Less,
            (Pre::Alpha(_), Pre::Num(_)) => std::cmp::Ordering::Greater,
            (Pre::Alpha(a), Pre::Alpha(b)) => a.cmp(b),
        }
    }
}

/// `major.minor.patch` with an optional `-pre.release` part, ordered as semver orders them. A
/// leading `v` is allowed and dropped.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Version {
    core: (u64, u64, u64),
    pre: Vec<Pre>,
}

impl Version {
    pub fn parse(text: &str) -> Option<Version> {
        let text = text.strip_prefix('v').unwrap_or(text);
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (text, None),
        };
        let number = |part: &str| -> Option<u64> {
            let plain = !part.is_empty()
                && part.bytes().all(|b| b.is_ascii_digit())
                && (part.len() == 1 || !part.starts_with('0'));
            if plain { part.parse().ok() } else { None }
        };
        let mut parts = core.split('.');
        let core = (
            number(parts.next()?)?,
            number(parts.next()?)?,
            number(parts.next()?)?,
        );
        if parts.next().is_some() {
            return None;
        }
        let pre = match pre {
            None => Vec::new(),
            Some(pre) => pre
                .split('.')
                .map(|id| {
                    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    {
                        None
                    } else if id.bytes().all(|b| b.is_ascii_digit()) {
                        number(id).map(Pre::Num)
                    } else {
                        Some(Pre::Alpha(id.to_owned()))
                    }
                })
                .collect::<Option<Vec<_>>>()?,
        };
        Some(Version { core, pre })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.core.cmp(&other.core).then_with(|| {
            // A release outranks its own pre-releases.
            match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => std::cmp::Ordering::Equal,
                (true, false) => std::cmp::Ordering::Greater,
                (false, true) => std::cmp::Ordering::Less,
                (false, false) => self.pre.cmp(&other.pre),
            }
        })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.core.0, self.core.1, self.core.2)?;
        for (i, id) in self.pre.iter().enumerate() {
            f.write_str(if i == 0 { "-" } else { "." })?;
            match id {
                Pre::Num(n) => write!(f, "{n}")?,
                Pre::Alpha(a) => f.write_str(a)?,
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub component: String,
    pub target: String,
    pub name: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub version: Version,
    pub artifacts: Vec<Artifact>,
}

impl Manifest {
    /// The build of `component` for `target`.
    pub fn artifact(&self, component: &str, target: &str) -> Option<&Artifact> {
        self.artifacts
            .iter()
            .find(|a| a.component == component && a.target == target)
    }

    /// The new signing key this release announces, if it does.
    pub fn key_artifact(&self) -> Option<&Artifact> {
        self.artifacts.iter().find(|a| a.component == KEY_COMPONENT)
    }
}

#[derive(Deserialize)]
struct RawManifest {
    version: String,
    artifacts: Vec<RawArtifact>,
}

#[derive(Deserialize)]
struct RawArtifact {
    component: String,
    target: String,
    name: String,
    sha256: String,
}

fn label(text: &str) -> bool {
    (1..=64).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Parses the manifest. Call it only on bytes whose signature has been verified.
fn parse(bytes: &[u8]) -> Result<Manifest, VerifyError> {
    let bad = |what: &str| VerifyError::Malformed(what.to_owned());
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(bad("too large"));
    }
    let raw: RawManifest = serde_json::from_slice(bytes).map_err(|e| bad(&e.to_string()))?;
    let version = Version::parse(&raw.version).ok_or_else(|| bad("version is not x.y.z"))?;
    if raw.artifacts.len() > MAX_ARTIFACTS {
        return Err(bad("too many artifacts"));
    }
    let mut artifacts: Vec<Artifact> = Vec::with_capacity(raw.artifacts.len());
    for a in raw.artifacts {
        if !label(&a.component) || !label(&a.target) {
            return Err(bad("component or target is not a plain name"));
        }
        if !crate::valid_file_name(&a.name)
            || a.name.starts_with('.')
            || a.name == MANIFEST_NAME
            || a.name == SIGNATURE_NAME
        {
            return Err(bad("an artifact name is not a plain file name"));
        }
        let hex = a.sha256.len() == 64
            && a.sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !hex {
            return Err(bad("sha256 is not 64 lower-case hex digits"));
        }
        if artifacts.iter().any(|x| x.name == a.name) {
            return Err(bad("two artifacts have the same name"));
        }
        artifacts.push(Artifact {
            component: a.component,
            target: a.target,
            name: a.name,
            sha256: a.sha256,
        });
    }
    Ok(Manifest { version, artifacts })
}

/// Checks `signature` (the text of the `.minisig` file) over `bytes` with `key` (the base64 line
/// of a minisign public key) and only then reads the manifest.
pub fn verify_manifest_with(
    key: &str,
    bytes: &[u8],
    signature: &str,
) -> Result<Manifest, VerifyError> {
    let key = key.trim();
    if key.is_empty() {
        return Err(VerifyError::NoKey);
    }
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(VerifyError::Malformed("too large".to_owned()));
    }
    let key = minisign_verify::PublicKey::from_base64(key).map_err(|_| VerifyError::BadKey)?;
    let signature =
        minisign_verify::Signature::decode(signature).map_err(|_| VerifyError::BadSignature)?;
    key.verify(bytes, &signature, false)
        .map_err(|_| VerifyError::BadSignature)?;
    parse(bytes)
}

/// The lower-case hex sha256 of everything `reader` gives.
pub fn sha256_hex(mut reader: impl Read) -> std::io::Result<String> {
    let mut hash = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(data_encoding::HEXLOWER.encode(&hash.finalize()))
}

/// The file at `path` has the sha256 the signed manifest gave.
pub fn verify_artifact(path: &Path, sha256: &str) -> Result<(), VerifyError> {
    let file = std::fs::File::open(path).map_err(|e| VerifyError::Io(e.to_string()))?;
    let got = sha256_hex(file).map_err(|e| VerifyError::Io(e.to_string()))?;
    if got == sha256 {
        Ok(())
    } else {
        Err(VerifyError::ChecksumMismatch)
    }
}

/// The base64 line of a minisign public key, from the text of a `.pub` file or from the line
/// alone.
pub fn parse_public_key(file: &[u8]) -> Result<String, VerifyError> {
    let text = std::str::from_utf8(file).map_err(|_| VerifyError::BadKey)?;
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("untrusted comment:"))
        .ok_or(VerifyError::BadKey)?;
    minisign_verify::PublicKey::from_base64(line).map_err(|_| VerifyError::BadKey)?;
    mtpool_protocol::signing::key_id(line).ok_or(VerifyError::BadKey)?;
    Ok(line.to_owned())
}

/// The new key a verified manifest announces. `key_file` is the file the manifest names; it must
/// have the signed checksum. `Err` when the manifest announces no key.
pub fn rotated_key(manifest: &Manifest, key_file: &[u8]) -> Result<String, VerifyError> {
    let artifact = manifest
        .key_artifact()
        .ok_or_else(|| VerifyError::Malformed("the release announces no new key".to_owned()))?;
    let got = sha256_hex(key_file).map_err(|e| VerifyError::Io(e.to_string()))?;
    if got != artifact.sha256 {
        return Err(VerifyError::ChecksumMismatch);
    }
    parse_public_key(key_file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(pair: &minisign::KeyPair, data: &[u8]) -> String {
        minisign::sign(Some(&pair.pk), &pair.sk, data, None, None)
            .unwrap()
            .into_string()
    }

    fn pair() -> minisign::KeyPair {
        minisign::KeyPair::generate_unencrypted_keypair().unwrap()
    }

    fn manifest_json(version: &str) -> Vec<u8> {
        format!(
            r#"{{"version":"{version}","artifacts":[{{"component":"mtpool-registrar","target":"x86_64-unknown-linux-musl","name":"mtpool-registrar-x86_64-unknown-linux-musl","sha256":"{}"}}]}}"#,
            "ab".repeat(32)
        )
        .into_bytes()
    }

    #[test]
    fn good_signature_gives_the_manifest() {
        let k = pair();
        let bytes = manifest_json("1.2.3");
        let m = verify_manifest_with(&k.pk.to_base64(), &bytes, &sign(&k, &bytes)).unwrap();
        assert_eq!(m.version, Version::parse("1.2.3").unwrap());
        assert_eq!(m.artifacts.len(), 1);
    }

    #[test]
    fn bad_signature_rejected() {
        let k = pair();
        let other = pair();
        let bytes = manifest_json("1.2.3");
        // Signed by somebody else.
        let e = verify_manifest_with(&k.pk.to_base64(), &bytes, &sign(&other, &bytes));
        assert!(matches!(e, Err(VerifyError::BadSignature)), "{e:?}");
        // The manifest changed after signing: one byte.
        let mut tampered = bytes.clone();
        tampered[12] = b'9';
        let e = verify_manifest_with(&k.pk.to_base64(), &tampered, &sign(&k, &bytes));
        assert!(matches!(e, Err(VerifyError::BadSignature)), "{e:?}");
        // Not a signature at all.
        let e = verify_manifest_with(&k.pk.to_base64(), &bytes, "nonsense");
        assert!(matches!(e, Err(VerifyError::BadSignature)), "{e:?}");
        // No key built in: nothing can be trusted.
        let e = verify_manifest_with("", &bytes, &sign(&k, &bytes));
        assert!(matches!(e, Err(VerifyError::NoKey)), "{e:?}");
    }

    #[test]
    fn signed_but_malformed_manifest_is_rejected() {
        let k = pair();
        for body in [
            &b"not json"[..],
            br#"{"version":"1.2","artifacts":[]}"#,
            br#"{"version":"1.2.3"}"#,
            br#"{"version":"1.2.3","artifacts":[{"component":"c","target":"t","name":"../x","sha256":"00"}]}"#,
        ] {
            let e = verify_manifest_with(&k.pk.to_base64(), body, &sign(&k, body));
            assert!(matches!(e, Err(VerifyError::Malformed(_))), "{body:?}: {e:?}");
        }
        let sha = "A".repeat(64);
        let upper = format!(
            r#"{{"version":"1.2.3","artifacts":[{{"component":"c","target":"t","name":"n","sha256":"{sha}"}}]}}"#
        );
        let e = verify_manifest_with(
            &k.pk.to_base64(),
            upper.as_bytes(),
            &sign(&k, upper.as_bytes()),
        );
        assert!(matches!(e, Err(VerifyError::Malformed(_))), "{e:?}");
    }

    #[test]
    fn tampered_artifact_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a");
        std::fs::write(&file, b"the build").unwrap();
        let sha = {
            use sha2::{Digest, Sha256};
            data_encoding::HEXLOWER.encode(&Sha256::digest(b"the build"))
        };
        verify_artifact(&file, &sha).unwrap();
        std::fs::write(&file, b"the build!").unwrap();
        let e = verify_artifact(&file, &sha);
        assert!(matches!(e, Err(VerifyError::ChecksumMismatch)), "{e:?}");
        let e = verify_artifact(&dir.path().join("missing"), &sha);
        assert!(matches!(e, Err(VerifyError::Io(_))), "{e:?}");
    }

    #[test]
    fn new_key_accepted_when_signed_by_old() {
        let old = pair();
        let new = pair();
        let key_file = new.pk.to_box().unwrap().into_string();
        let sha = {
            use sha2::{Digest, Sha256};
            data_encoding::HEXLOWER.encode(&Sha256::digest(key_file.as_bytes()))
        };
        let bytes = format!(
            r#"{{"version":"2.0.0","artifacts":[{{"component":"mtpool-signing-key","target":"any","name":"minisign.pub","sha256":"{sha}"}}]}}"#
        )
        .into_bytes();
        // The release that brings the new key is signed by the old one, and the old key accepts it.
        let m = verify_manifest_with(&old.pk.to_base64(), &bytes, &sign(&old, &bytes)).unwrap();
        let announced = rotated_key(&m, key_file.as_bytes()).unwrap();
        assert_eq!(announced, new.pk.to_base64());
        // From then on the new key is the one: a release signed by it is accepted, one signed by
        // the old key is not.
        let next = manifest_json("2.0.1");
        verify_manifest_with(&announced, &next, &sign(&new, &next)).unwrap();
        let e = verify_manifest_with(&announced, &next, &sign(&old, &next));
        assert!(matches!(e, Err(VerifyError::BadSignature)), "{e:?}");
        // A key file that is not the one the signed manifest names is refused.
        let e = rotated_key(
            &m,
            new.pk
                .to_box()
                .unwrap()
                .into_string()
                .replace('R', "S")
                .as_bytes(),
        );
        assert!(e.is_err());
        // A key nobody announced, signed by a key nobody trusts: the old key refuses.
        let stranger = pair();
        let e = verify_manifest_with(&old.pk.to_base64(), &bytes, &sign(&stranger, &bytes));
        assert!(matches!(e, Err(VerifyError::BadSignature)), "{e:?}");
    }

    #[test]
    fn versions_compare_like_semver() {
        let v = |t: &str| Version::parse(t).unwrap();
        assert!(v("1.2.4") > v("1.2.3"));
        assert!(v("1.10.0") > v("1.9.9"));
        assert!(v("1.2.3") == v("v1.2.3"));
        assert!(v("1.2.3") > v("1.2.3-rc.1"));
        assert!(v("1.2.3-rc.10") > v("1.2.3-rc.2"));
        assert!(v("1.2.3-beta") > v("1.2.3-alpha.9"));
        assert!(Version::parse("1.2").is_none());
        assert!(Version::parse("1.2.x").is_none());
        assert!(Version::parse("").is_none());
        assert!(Version::parse("01.2.3").is_none());
    }
}
