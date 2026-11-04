//! What the registrar and the agent share when they replace their own binary: the signed
//! release manifest and the checks before anything in it is trusted, the staging and swap of
//! the binary with the previous build kept beside it, the probation marker and the list of
//! releases that were rolled back.

pub mod manifest;

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use manifest::Version;

pub const MANIFEST_NAME: &str = "manifest.json";
pub const SIGNATURE_NAME: &str = "manifest.json.minisig";
/// One build is never bigger than this.
pub const MAX_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;
/// The longest release file name.
pub const MAX_NAME_LEN: usize = 120;

/// A release file name: letters, digits, dot, dash and underscore, not made of dots alone.
pub fn valid_file_name(name: &str) -> bool {
    (1..=MAX_NAME_LEN).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && name.bytes().any(|b| b != b'.')
}

/// Where the updater reads and writes.
#[derive(Debug, Clone)]
pub struct Paths {
    /// The releases directory the registrar serves agents from.
    pub releases: PathBuf,
    /// Root-owned state: what is installed, what is rejected, the probation marker.
    pub state: PathBuf,
}

/// The marker that says a new build is on trial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probation {
    pub version: Version,
    pub previous: Version,
}

const INSTALLED_FILE: &str = "installed";
const REJECTED_FILE: &str = "rejected";
const PROBATION_FILE: &str = "probation";

/// Writes `text` to `path` so that a reader sees the old file or the whole new one.
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

impl Paths {
    /// The version the helper last confirmed as running.
    pub fn installed(&self) -> Option<Version> {
        std::fs::read_to_string(self.state.join(INSTALLED_FILE))
            .ok()
            .and_then(|t| Version::parse(t.trim()))
    }

    pub fn set_installed(&self, version: &Version) -> std::io::Result<()> {
        write_atomic(&self.state.join(INSTALLED_FILE), &format!("{version}\n"))
    }

    /// Releases that were rolled back, with the reason.
    pub fn rejected(&self) -> Vec<(Version, String)> {
        let text = std::fs::read_to_string(self.state.join(REJECTED_FILE)).unwrap_or_default();
        text.lines()
            .filter_map(|line| {
                let (version, reason) = line.split_once(' ').unwrap_or((line, ""));
                Some((Version::parse(version)?, reason.to_owned()))
            })
            .collect()
    }

    pub fn is_rejected(&self, version: &Version) -> bool {
        self.rejected().iter().any(|(v, _)| v == version)
    }

    pub fn reject(&self, version: &Version, reason: &str) -> std::io::Result<()> {
        use std::io::Write;
        let reason = reason.replace(['\n', '\r'], " ");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.state.join(REJECTED_FILE))?;
        writeln!(file, "{version} {reason}")
    }

    pub fn probation(&self) -> Option<Probation> {
        let text = std::fs::read_to_string(self.state.join(PROBATION_FILE)).ok()?;
        let (version, previous) = text.trim().split_once(' ')?;
        Some(Probation {
            version: Version::parse(version)?,
            previous: Version::parse(previous)?,
        })
    }

    pub fn begin_probation(&self, version: &Version, previous: &Version) -> std::io::Result<()> {
        write_atomic(
            &self.state.join(PROBATION_FILE),
            &format!("{version} {previous}\n"),
        )
    }

    /// Takes the marker away, and is the one caller that gets it: the confirmation of a good
    /// build and the rollback of a bad one cannot both happen.
    pub fn claim_probation(&self) -> Option<Probation> {
        let marker = self.probation()?;
        let claimed = self
            .state
            .join(format!("{PROBATION_FILE}.claimed{}", std::process::id()));
        std::fs::rename(self.state.join(PROBATION_FILE), &claimed).ok()?;
        let _ = std::fs::remove_file(claimed);
        Some(marker)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Ready,
    Failed,
}

/// `path` with `suffix` appended to its file name.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// The file the previous build is kept in beside the installed one.
pub fn previous_of(bin: &Path) -> PathBuf {
    sibling(bin, ".prev")
}

/// The file a build is staged in before it replaces the installed one.
pub fn staged_of(bin: &Path) -> PathBuf {
    sibling(bin, ".new")
}

/// Makes `staged` the installed binary and keeps the build that was installed as `<bin>.prev`.
///
/// `bin` is never missing and never half-written: the old build is linked to its `.prev` name
/// (through a temporary name, so an older `.prev` is replaced in one step) and the staged file is
/// renamed over `bin`. A running process keeps the inode it started from.
pub fn replace_binary(bin: &Path, staged: &Path) -> io::Result<()> {
    let prev = previous_of(bin);
    let tmp = sibling(bin, ".prev.tmp");
    let _ = std::fs::remove_file(&tmp);
    if bin.exists() {
        if std::fs::hard_link(bin, &tmp).is_err() {
            std::fs::copy(bin, &tmp)?;
        }
        std::fs::rename(&tmp, &prev)?;
    }
    std::fs::rename(staged, bin)
}

/// Probes until `window` is over. `Ready` needs at least one good answer and no bad one after
/// the first good one; a build that answered and then stopped is not a good build.
pub async fn watch(
    window: Duration,
    every: Duration,
    mut probe: impl AsyncFnMut() -> bool,
) -> Verdict {
    let started = tokio::time::Instant::now();
    let mut seen = false;
    while started.elapsed() < window {
        if probe().await {
            seen = true;
        } else if seen {
            return Verdict::Failed;
        }
        tokio::time::sleep(every).await;
    }
    if seen {
        Verdict::Ready
    } else {
        Verdict::Failed
    }
}

/// The regular file `name` in the releases directory, read whole up to `max` bytes. The directory
/// belongs to an unprivileged user, so a link in it is not followed.
pub fn read_release_file(dir: &Path, name: &str, max: u64) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut file = open_regular(&dir.join(name), max)?;
    let mut body = Vec::new();
    file.by_ref().take(max + 1).read_to_end(&mut body)?;
    Ok(body)
}

pub fn open_regular(path: &Path, max: u64) -> io::Result<std::fs::File> {
    use std::os::unix::fs::MetadataExt;
    let before = std::fs::symlink_metadata(path)?;
    if !before.is_file() || before.len() > max {
        return Err(io::Error::other(format!(
            "{} is not a regular file of acceptable size",
            path.display()
        )));
    }
    let file = std::fs::File::open(path)?;
    let after = file.metadata()?;
    if before.ino() != after.ino() || before.dev() != after.dev() {
        return Err(io::Error::other(format!(
            "{} changed while opening",
            path.display()
        )));
    }
    Ok(file)
}

/// Copies the build into the staging file while hashing the bytes that are written, so that what
/// is checked is what is installed.
pub fn stage(source: &Path, staged: &Path, want_sha256: &str) -> Result<(), String> {
    use std::io::{Read, Write};
    use std::os::unix::fs::OpenOptionsExt;
    let mut from = open_regular(source, MAX_ARTIFACT_BYTES).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(staged);
    let mut to = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(staged)
        .map_err(|e| format!("cannot create {}: {e}", staged.display()))?;
    let mut hash = <sha2::Sha256 as sha2::Digest>::new();
    let mut buf = vec![0u8; 64 * 1024];
    let copied: io::Result<()> = (|| {
        loop {
            let n = from.read(&mut buf)?;
            if n == 0 {
                break;
            }
            sha2::Digest::update(&mut hash, &buf[..n]);
            to.write_all(&buf[..n])?;
        }
        to.sync_all()
    })();
    let got = data_encoding::HEXLOWER.encode(&sha2::Digest::finalize(hash));
    if let Err(e) = copied {
        let _ = std::fs::remove_file(staged);
        return Err(format!("cannot copy the build: {e}"));
    }
    if got != want_sha256 {
        let _ = std::fs::remove_file(staged);
        return Err("the build does not have the checksum the signed manifest gives".to_owned());
    }
    Ok(())
}

/// How long a just-written build may stay busy before it counts as not runnable.
const BUSY_WAIT: Duration = Duration::from_secs(5);

/// Starts `<path> version`, waiting while the file is busy.
///
/// A file that is open for writing anywhere cannot be run (ETXTBSY), and closing our own handle
/// is not enough: a thread of this process that forks while the file is being written gives its
/// child a copy of the handle, and the child holds it until its exec. That lasts a moment and
/// says nothing about the build.
fn spawn_version(path: &Path, busy_wait: Duration) -> io::Result<std::process::Child> {
    use std::process::{Command, Stdio};
    let started = std::time::Instant::now();
    loop {
        let spawned = Command::new(path)
            .arg("version")
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            Err(e)
                if e.kind() == io::ErrorKind::ExecutableFileBusy
                    && started.elapsed() < busy_wait =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            other => return other,
        }
    }
}

/// The key id in the output of `mtpool-registrar version` or `mtpool-agent version`: `1.2.3 (signing key E7620F1842B4E81F)`.
pub fn key_id_of_version_line(line: &str) -> Option<String> {
    let rest = line.split_once("(signing key ")?.1;
    let id = rest.split_once(')')?.0.trim();
    (id.len() == 16 && id.bytes().all(|b| b.is_ascii_hexdigit())).then(|| id.to_owned())
}

/// Runs `path version` and reads the id of the signing key built into it off the output.
pub fn binary_key_id(path: &Path) -> Result<Option<String>, String> {
    use std::io::Read;
    let mut child = spawn_version(path, BUSY_WAIT).map_err(|e| e.to_string())?;
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => break status,
            None if started.elapsed() > Duration::from_secs(10) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("`version` did not finish in 10 seconds".to_owned());
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    if !status.success() {
        return Err(format!("`version` exited with {status}"));
    }
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.by_ref().take(4096).read_to_string(&mut out);
    }
    Ok(key_id_of_version_line(&out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A build that answers `version`, plus a handle that still has it open for writing.
    fn build_being_written(dir: &Path) -> (PathBuf, std::fs::File) {
        let path = dir.join("build");
        std::fs::write(
            &path,
            "#!/bin/sh\necho '1.2.3 (signing key E7620F1842B4E81F)'\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let writer = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        (path, writer)
    }

    #[test]
    fn a_build_that_is_busy_for_a_moment_still_answers() {
        let dir = tempfile::tempdir().unwrap();
        let (path, writer) = build_being_written(dir.path());
        let closer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(writer);
        });

        let got = binary_key_id(&path);

        closer.join().unwrap();
        assert_eq!(got, Ok(Some("E7620F1842B4E81F".to_owned())));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_build_that_stays_busy_is_not_waited_for_forever() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _writer) = build_being_written(dir.path());

        let got = spawn_version(&path, Duration::from_millis(50));

        assert_eq!(got.unwrap_err().kind(), io::ErrorKind::ExecutableFileBusy);
    }
}
