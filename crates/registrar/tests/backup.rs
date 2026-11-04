//! Backup and restore of the registrar database.

mod common;

use std::fs;
use std::path::Path;

use mtpool_registrar::backup::{self, BackupError};
use rusqlite::Connection;

use common::temp_db;

/// A database with the schema and a few owners in it, the way the registrar leaves it.
fn seeded(path: &Path) {
    let store = mtpool_registrar::store::Store::open(path).expect("open store");
    store.close().expect("close store");
    let db = Connection::open(path).expect("open");
    for (id, name) in [(1, "alpha"), (2, "beta"), (3, "gamma")] {
        db.execute(
            "INSERT INTO owners (id, name, node_limit, created_at) VALUES (?1, ?2, 5, 1000)",
            (id, name),
        )
        .expect("insert");
    }
}

fn owners(path: &Path) -> Vec<(i64, String)> {
    let db = Connection::open(path).expect("open");
    let mut stmt = db
        .prepare("SELECT id, name FROM owners ORDER BY id")
        .expect("prepare");
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query")
        .map(|r| r.expect("row"))
        .collect()
}

#[test]
fn backup_writes_checksum() {
    let (dir, db) = temp_db();
    seeded(&db);
    let to = dir.path().join("out").join("copy.db");

    backup::backup(&db, &to).expect("backup");

    assert_eq!(owners(&to).len(), 3, "the copy holds the rows");
    let sum = fs::read_to_string(dir.path().join("out").join("copy.db.sha256")).expect("checksum");
    let (hex, name) = sum.trim_end().split_once("  ").expect("sha256sum format");
    assert_eq!(hex.len(), 64, "{sum}");
    assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()), "{sum}");
    assert_eq!(
        name, "copy.db",
        "the checksum names the file by its base name"
    );
}

#[test]
fn restore_refuses_bad_checksum() {
    let (dir, db) = temp_db();
    seeded(&db);
    let to = dir.path().join("copy.db");
    backup::backup(&db, &to).expect("backup");

    // The live database changes after the backup; a refused restore must leave it as it is.
    Connection::open(&db)
        .expect("open")
        .execute("DELETE FROM owners WHERE id = 3", [])
        .expect("delete");

    // A wrong checksum.
    fs::write(
        dir.path().join("copy.db.sha256"),
        format!("{}  copy.db\n", "0".repeat(64)),
    )
    .expect("write");
    let error = backup::restore(&db, &to).expect_err("a wrong checksum is refused");
    assert!(matches!(error, BackupError::Checksum(_)), "{error}");
    assert_eq!(owners(&db).len(), 2, "nothing was touched");

    // No checksum at all.
    fs::remove_file(dir.path().join("copy.db.sha256")).expect("remove");
    let error = backup::restore(&db, &to).expect_err("a missing checksum is refused");
    assert!(matches!(error, BackupError::Checksum(_)), "{error}");
    assert_eq!(owners(&db).len(), 2, "nothing was touched");
}

#[test]
fn restore_refuses_while_locked() {
    let (dir, db) = temp_db();
    seeded(&db);
    let to = dir.path().join("copy.db");
    backup::backup(&db, &to).expect("backup");

    let serving = backup::lock_serving(&db).expect("the first lock");
    let error = backup::restore(&db, &to).expect_err("a serving registrar blocks the restore");
    assert!(matches!(error, BackupError::Running), "{error}");
    assert!(
        error
            .to_string()
            .contains("the registrar is running; stop it first"),
        "{error}"
    );
    assert!(
        backup::lock_serving(&db).is_err(),
        "a second registrar cannot serve the same database"
    );

    drop(serving);
    backup::restore(&db, &to).expect("restore once the registrar has stopped");
}

#[test]
fn backup_then_restore_equal_rows() {
    let (dir, db) = temp_db();
    seeded(&db);
    let before = owners(&db);
    let to = dir.path().join("copy.db");
    backup::backup(&db, &to).expect("backup");

    // The database is damaged and a stale WAL is lying beside it.
    Connection::open(&db)
        .expect("open")
        .execute("DELETE FROM owners", [])
        .expect("delete");
    fs::write(dir.path().join("registrar.db-wal"), b"stale").expect("wal");
    fs::write(dir.path().join("registrar.db-shm"), b"stale").expect("shm");

    backup::restore(&db, &to).expect("restore");

    assert_eq!(owners(&db), before);
    assert!(!dir.path().join("registrar.db-wal").exists());
    assert!(!dir.path().join("registrar.db-shm").exists());
    // The restored database opens like any other.
    mtpool_registrar::store::Store::open(&db)
        .expect("open the restored store")
        .close()
        .expect("close");
}

#[test]
fn backup_works_while_the_store_serves() {
    let (dir, db) = temp_db();
    let store = mtpool_registrar::store::Store::open(&db).expect("open store");
    let to = dir.path().join("live.db");
    backup::backup(&db, &to).expect("backup of a database in use");
    store.close().expect("close");
    assert!(to.exists());
}

#[test]
fn restore_leaves_the_files_with_the_owner_of_their_directory() {
    use std::os::unix::fs::MetadataExt;

    let (dir, db) = temp_db();
    seeded(&db);
    let to = dir.path().join("copy.db");
    backup::backup(&db, &to).expect("backup");
    backup::restore(&db, &to).expect("restore");

    let wanted = fs::metadata(dir.path()).expect("dir");
    for file in [db.clone(), backup::lock_path(&db)] {
        let meta = fs::metadata(&file).expect("file exists");
        assert_eq!(
            (meta.uid(), meta.gid()),
            (wanted.uid(), wanted.gid()),
            "{}",
            file.display()
        );
    }
    // Run on its own, as a no-op when the files already match (and when not root).
    backup::match_directory_owner(&db).expect("owner already matches");
}

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

#[test]
fn database_and_copies_are_private() {
    let (dir, db) = temp_db();
    seeded(&db);
    assert_eq!(mode(&db), 0o600, "a new database is private");

    // An install made before this rule left it readable: opening heals it, and so does a store
    // that is still open (its -wal and -shm are made with the mode of the database).
    fs::set_permissions(&db, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    let store = mtpool_registrar::store::Store::open(&db).expect("open store");
    assert_eq!(mode(&db), 0o600, "opening heals an old database");
    store.flush().expect("flush");
    let wal = std::path::PathBuf::from(format!("{}-wal", db.display()));
    assert!(wal.exists(), "the check below looks at a real -wal");
    for suffix in ["-wal", "-shm"] {
        let side = std::path::PathBuf::from(format!("{}{suffix}", db.display()));
        if side.exists() {
            assert_eq!(mode(&side), 0o600, "{}", side.display());
        }
    }
    store.close().expect("close");

    let to = dir.path().join("out").join("copy.db");
    backup::backup(&db, &to).expect("backup");
    assert_eq!(mode(&to), 0o600, "the backup is private");
    assert_eq!(mode(&dir.path().join("out").join("copy.db.sha256")), 0o600);

    fs::set_permissions(&to, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    backup::restore(&db, &to).expect("restore");
    assert_eq!(mode(&db), 0o600, "the restored database is private");
}
