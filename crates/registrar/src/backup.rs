//! Backup and restore of the database.
//!
//! A backup is a consistent copy made with the SQLite online backup API, so it works while the
//! registrar serves, and a `.sha256` file beside it in the format `sha256sum -c` reads. A restore
//! checks that file, refuses while a registrar serves (the serving process holds an advisory lock
//! on `<db>.lock`) and replaces the database only with a copy that passes `integrity_check`.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use data_encoding::HEXLOWER;
use rusqlite::backup::Backup;
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("the registrar is running; stop it first")]
    Running,
    #[error("{0}")]
    Checksum(String),
    #[error("{what}: {source}")]
    Io {
        what: String,
        #[source]
        source: io::Error,
    },
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("the backup is damaged: integrity_check said {0}")]
    Integrity(String),
}

fn io_error(what: impl Into<String>) -> impl FnOnce(io::Error) -> BackupError {
    let what = what.into();
    move |source| BackupError::Io { what, source }
}

/// `<path>` with `suffix` appended to the file name.
fn beside(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// The lock file of the database at `db`.
pub fn lock_path(db: &Path) -> PathBuf {
    beside(db, ".lock")
}

fn open_lock(db: &Path) -> Result<File, BackupError> {
    let path = lock_path(db);
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir).map_err(io_error(format!("cannot create {}", dir.display())))?;
    }
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(io_error(format!("cannot open {}", path.display())))
}

/// Takes the lock a serving registrar holds for as long as it runs; dropping the returned file
/// releases it. Fails when another process holds it.
pub fn lock_serving(db: &Path) -> Result<File, BackupError> {
    let file = open_lock(db)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(BackupError::Running),
        Err(TryLockError::Error(source)) => Err(BackupError::Io {
            what: format!("cannot lock {}", lock_path(db).display()),
            source,
        }),
    }
}

/// The SHA-256 of a file as lower-case hex.
fn sha256_of(path: &Path) -> Result<String, BackupError> {
    let mut file = File::open(path).map_err(io_error(format!("cannot read {}", path.display())))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buffer)
            .map_err(io_error(format!("cannot read {}", path.display())))?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(HEXLOWER.encode(&hasher.finalize()))
}

/// Makes `path` readable and writable by its owner only: created that way when it is not there
/// yet, and narrowed when it is. A caller that does not own the file may not narrow it; that is
/// left alone, as `match_directory_owner` leaves an owner it may not change.
pub fn make_private(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    match OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
    {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {}
        Err(e) => return Err(e),
    }
    match fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => Ok(()),
        other => other,
    }
}

fn base_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(|| "backup".to_owned(), |n| n.to_string_lossy().into_owned())
}

/// Copies the database at `db` to `to` and writes `<to>.sha256`. The copy is consistent even
/// while the registrar writes: it is made by the SQLite backup API from a fresh connection.
pub fn backup(db: &Path, to: &Path) -> Result<(), BackupError> {
    if let Some(dir) = to.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir).map_err(io_error(format!("cannot create {}", dir.display())))?;
    }
    // The copy is written beside its final name and moved into place whole.
    let partial = beside(to, ".partial");
    let _ = fs::remove_file(&partial);
    let result = copy_database(db, &partial).and_then(|()| {
        fs::rename(&partial, to).map_err(io_error(format!(
            "cannot move the backup to {}",
            to.display()
        )))
    });
    if result.is_err() {
        let _ = fs::remove_file(&partial);
        return result;
    }
    let sum = sha256_of(to)?;
    let sum_path = beside(to, ".sha256");
    make_private(&sum_path).map_err(io_error(format!("cannot write {}", sum_path.display())))?;
    let mut file = File::create(&sum_path)
        .map_err(io_error(format!("cannot write {}", sum_path.display())))?;
    writeln!(file, "{sum}  {}", base_name(to))
        .and_then(|()| file.sync_all())
        .map_err(io_error(format!("cannot write {}", sum_path.display())))
}

fn copy_database(db: &Path, to: &Path) -> Result<(), BackupError> {
    let source = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    make_private(to).map_err(io_error(format!("cannot create {}", to.display())))?;
    source.busy_timeout(Duration::from_secs(5))?;
    let mut target = Connection::open(to)?;
    {
        let copy = Backup::new(&source, &mut target)?;
        copy.run_to_completion(256, Duration::from_millis(5), None)?;
    }
    // The copy is a single file: no WAL is left to travel with it.
    target.pragma_update(None, "journal_mode", "DELETE")?;
    target
        .close()
        .map_err(|(_, error)| BackupError::Sqlite(error))
}

/// The checksum the file `<from>.sha256` promises, or why there is none to trust.
fn promised_checksum(from: &Path) -> Result<String, BackupError> {
    let sum_path = beside(from, ".sha256");
    let text = fs::read_to_string(&sum_path).map_err(|e| {
        BackupError::Checksum(format!(
            "cannot read the checksum {}: {e}; restore only a backup with its .sha256 file",
            sum_path.display()
        ))
    })?;
    let first = text.split_whitespace().next().unwrap_or("");
    if first.len() != 64 || !first.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(BackupError::Checksum(format!(
            "{} does not hold a SHA-256 checksum",
            sum_path.display()
        )));
    }
    Ok(first.to_ascii_lowercase())
}

/// Replaces the database at `db` with the backup `from`. Refuses when the checksum file is
/// missing or does not match, when a registrar serves the database, or when the copy fails
/// `integrity_check`; in each case the database is left as it was.
pub fn restore(db: &Path, from: &Path) -> Result<(), BackupError> {
    let promised = promised_checksum(from)?;
    let actual = sha256_of(from)?;
    if actual != promised {
        return Err(BackupError::Checksum(format!(
            "the checksum of {} is {actual}, the .sha256 file says {promised}",
            from.display()
        )));
    }
    // Held until the new database is in place, so a registrar cannot start halfway.
    let _lock = lock_serving(db)?;

    let staged = beside(db, ".restoring");
    let _ = fs::remove_file(&staged);
    let staged_result = fs::copy(from, &staged)
        .map_err(io_error(format!("cannot copy {}", from.display())))
        .and_then(|_| {
            make_private(&staged).map_err(io_error(format!("cannot protect {}", staged.display())))
        })
        .and_then(|()| check_integrity(&staged));
    if let Err(error) = staged_result {
        let _ = fs::remove_file(&staged);
        return Err(error);
    }
    for suffix in ["-wal", "-shm"] {
        let stale = beside(db, suffix);
        match fs::remove_file(&stale) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                let _ = fs::remove_file(&staged);
                return Err(BackupError::Io {
                    what: format!("cannot remove {}", stale.display()),
                    source: e,
                });
            }
        }
    }
    fs::rename(&staged, db).map_err(io_error(format!("cannot replace {}", db.display())))?;
    match_directory_owner(db)
}

/// Gives the database and its lock file the owner of the directory they live in. A restore run
/// as root would otherwise leave them root-owned, and the registrar (user `mtpool`) could not
/// open them. It changes nothing when the owner already matches, and when the caller may not
/// change owners (anyone but root).
pub fn match_directory_owner(db: &Path) -> Result<(), BackupError> {
    use std::os::unix::fs::{MetadataExt, chown};

    let dir = db.parent().filter(|p| !p.as_os_str().is_empty());
    let dir = dir.unwrap_or_else(|| Path::new("."));
    let wanted = fs::metadata(dir).map_err(io_error(format!("cannot read {}", dir.display())))?;
    for file in [db.to_path_buf(), lock_path(db)] {
        let meta = match fs::metadata(&file) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(BackupError::Io {
                    what: format!("cannot read {}", file.display()),
                    source: e,
                });
            }
        };
        if (meta.uid(), meta.gid()) == (wanted.uid(), wanted.gid()) {
            continue;
        }
        match chown(&file, Some(wanted.uid()), Some(wanted.gid())) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {}
            Err(e) => {
                return Err(BackupError::Io {
                    what: format!("cannot change the owner of {}", file.display()),
                    source: e,
                });
            }
        }
    }
    Ok(())
}

fn check_integrity(path: &Path) -> Result<(), BackupError> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let verdict: String = db.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if verdict == "ok" {
        Ok(())
    } else {
        Err(BackupError::Integrity(verdict))
    }
}
