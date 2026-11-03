//! The state the agent keeps between runs: `node_id`, `node_token` and `enroll_token`, each in
//! its own file in the state directory with mode 0600. All access goes through [`Fs`] so tests
//! can run against memory.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const NODE_ID_FILE: &str = "node_id";
pub const NODE_TOKEN_FILE: &str = "node_token";
pub const ENROLL_TOKEN_FILE: &str = "enroll_token";

/// The few file operations the agent needs.
pub trait Fs: Send + Sync {
    /// The content of the file, `None` if it does not exist.
    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>>;
    /// Replaces the file with `bytes`, readable by the owner only. The content is written to
    /// `<path>.tmp` first and renamed, so a crash never leaves half a file.
    fn write_private(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    /// Removes the file; one that is not there is not an error.
    fn remove(&self, path: &Path) -> io::Result<()>;
}

/// The real file system.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealFs;

impl Fs for RealFs {
    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn write_private(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

        let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
        if let Some(parent) = parent {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        // A leftover from a crash may have other permissions: start from nothing.
        match std::fs::remove_file(&tmp) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let written = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            file.write_all(bytes)?;
            file.sync_all()
        })();
        if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, path)) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        if let Some(parent) = parent {
            // Make the rename durable; a directory that cannot be synced is not an error.
            if let Ok(dir) = std::fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        Ok(())
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// Files in memory, for tests.
#[derive(Debug, Default)]
pub struct MemFs {
    files: Mutex<BTreeMap<PathBuf, Vec<u8>>>,
}

impl MemFs {
    pub fn new() -> MemFs {
        MemFs::default()
    }

    fn files(&self) -> std::sync::MutexGuard<'_, BTreeMap<PathBuf, Vec<u8>>> {
        self.files
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Fs for MemFs {
    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        Ok(self.files().get(path).cloned())
    }
    fn write_private(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.files().insert(path.to_owned(), bytes.to_vec());
        Ok(())
    }
    fn remove(&self, path: &Path) -> io::Result<()> {
        self.files().remove(path);
        Ok(())
    }
}

/// A token. It prints as `<redacted>` so it cannot leak through `{:?}` into a log.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Secret {
        Secret(value.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// What a registered node holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub node_id: String,
    pub node_token: Secret,
}

/// The three state files of one agent.
#[derive(Clone)]
pub struct StateFiles {
    fs: Arc<dyn Fs>,
    dir: PathBuf,
}

impl StateFiles {
    pub fn new(fs: Arc<dyn Fs>, dir: PathBuf) -> StateFiles {
        StateFiles { fs, dir }
    }
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    pub fn node_id_path(&self) -> PathBuf {
        self.dir.join(NODE_ID_FILE)
    }
    pub fn node_token_path(&self) -> PathBuf {
        self.dir.join(NODE_TOKEN_FILE)
    }
    pub fn enroll_token_path(&self) -> PathBuf {
        self.dir.join(ENROLL_TOKEN_FILE)
    }
    /// The node id and token if both are there.
    pub fn load_credentials(&self) -> io::Result<Option<Credentials>> {
        let id = self.read_text(&self.node_id_path())?;
        let token = self.read_text(&self.node_token_path())?;
        Ok(match (id, token) {
            (Some(node_id), Some(token)) => Some(Credentials {
                node_id,
                node_token: Secret::new(token),
            }),
            _ => None,
        })
    }
    /// Writes the token first and the id second: a crash in between leaves no id, which the
    /// next start reads as "not enrolled".
    pub fn save_credentials(&self, credentials: &Credentials) -> io::Result<()> {
        self.fs.write_private(
            &self.node_token_path(),
            credentials.node_token.expose().as_bytes(),
        )?;
        self.fs
            .write_private(&self.node_id_path(), credentials.node_id.as_bytes())
    }
    /// Removes the node token only (the registrar said 401).
    pub fn remove_node_token(&self) -> io::Result<()> {
        self.fs.remove(&self.node_token_path())
    }
    /// Removes the node id and the node token (the registrar does not know the node: 410).
    pub fn clear_credentials(&self) -> io::Result<()> {
        // The token first, for the same reason it is written first.
        let token = self.fs.remove(&self.node_token_path());
        let id = self.fs.remove(&self.node_id_path());
        token.and(id)
    }
    /// The enroll token written by the installer; `None` if the file is missing or empty.
    pub fn enroll_token(&self) -> io::Result<Option<Secret>> {
        Ok(self.read_text(&self.enroll_token_path())?.map(Secret::new))
    }

    /// The trimmed text of a file; `None` if it is missing or blank.
    fn read_text(&self, path: &Path) -> io::Result<Option<String>> {
        let Some(bytes) = self.fs.read(path)? else {
            return Ok(None);
        };
        let text = String::from_utf8(bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "the file is not valid UTF-8 text",
            )
        })?;
        let text = text.trim();
        Ok((!text.is_empty()).then(|| text.to_owned()))
    }
}
