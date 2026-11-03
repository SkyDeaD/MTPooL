use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use mtpool_agent::state_files::{Credentials, Fs, MemFs, RealFs, Secret, StateFiles};

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn real_fs_writes_private_files_and_replaces_them() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a/b/token");
    RealFs.write_private(&file, b"one").unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), b"one");
    assert_eq!(mode(&file), 0o600);
    assert_eq!(mode(file.parent().unwrap()), 0o700);

    // A stale temporary file with loose permissions does not leak into the result.
    let tmp = dir.path().join("a/b/token.tmp");
    std::fs::write(&tmp, b"stale").unwrap();
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
    RealFs.write_private(&file, b"two").unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), b"two");
    assert_eq!(mode(&file), 0o600);
    assert!(!tmp.exists());
}

#[test]
fn real_fs_reads_and_removes() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("x");
    assert_eq!(RealFs.read(&file).unwrap(), None);
    RealFs.remove(&file).unwrap();
    RealFs.write_private(&file, b"v").unwrap();
    assert_eq!(RealFs.read(&file).unwrap().as_deref(), Some(&b"v"[..]));
    RealFs.remove(&file).unwrap();
    assert_eq!(RealFs.read(&file).unwrap(), None);
}

#[test]
fn mem_fs_behaves_like_the_real_one() {
    let fs = MemFs::new();
    let file = Path::new("/s/x");
    assert_eq!(fs.read(file).unwrap(), None);
    fs.remove(file).unwrap();
    fs.write_private(file, b"v").unwrap();
    fs.write_private(file, b"w").unwrap();
    assert_eq!(fs.read(file).unwrap().as_deref(), Some(&b"w"[..]));
    fs.remove(file).unwrap();
    assert_eq!(fs.read(file).unwrap(), None);
}

fn state(fs: &Arc<MemFs>) -> StateFiles {
    StateFiles::new(fs.clone(), "/s".into())
}

#[test]
fn credentials_round_trip_and_need_both_files() {
    let fs = Arc::new(MemFs::new());
    let files = state(&fs);
    assert_eq!(files.load_credentials().unwrap(), None);

    let credentials = Credentials {
        node_id: "alpha-a1b2c".into(),
        node_token: Secret::new("tok"),
    };
    files.save_credentials(&credentials).unwrap();
    assert_eq!(files.load_credentials().unwrap(), Some(credentials));
    assert_eq!(
        fs.read(&files.node_id_path()).unwrap().as_deref(),
        Some(&b"alpha-a1b2c"[..])
    );
    assert_eq!(
        fs.read(&files.node_token_path()).unwrap().as_deref(),
        Some(&b"tok"[..])
    );

    files.remove_node_token().unwrap();
    assert_eq!(files.load_credentials().unwrap(), None);
    assert!(fs.read(&files.node_id_path()).unwrap().is_some());

    files
        .save_credentials(&Credentials {
            node_id: "a-00000".into(),
            node_token: Secret::new("t"),
        })
        .unwrap();
    files.clear_credentials().unwrap();
    assert_eq!(fs.read(&files.node_id_path()).unwrap(), None);
    assert_eq!(fs.read(&files.node_token_path()).unwrap(), None);
}

#[test]
fn stored_values_are_trimmed_and_blank_ones_are_missing() {
    let fs = Arc::new(MemFs::new());
    let files = state(&fs);
    fs.write_private(&files.enroll_token_path(), b"  secret-value\n")
        .unwrap();
    assert_eq!(
        files.enroll_token().unwrap().unwrap().expose(),
        "secret-value"
    );
    fs.write_private(&files.enroll_token_path(), b"\n").unwrap();
    assert_eq!(files.enroll_token().unwrap(), None);
    fs.remove(&files.enroll_token_path()).unwrap();
    assert_eq!(files.enroll_token().unwrap(), None);

    fs.write_private(&files.node_id_path(), b"alpha-a1b2c\n")
        .unwrap();
    fs.write_private(&files.node_token_path(), b"tok\n")
        .unwrap();
    let loaded = files.load_credentials().unwrap().unwrap();
    assert_eq!(
        (loaded.node_id.as_str(), loaded.node_token.expose()),
        ("alpha-a1b2c", "tok")
    );
}

#[test]
fn a_file_that_is_not_text_is_an_error() {
    let fs = Arc::new(MemFs::new());
    let files = state(&fs);
    fs.write_private(&files.enroll_token_path(), &[0xff, 0xfe])
        .unwrap();
    assert!(files.enroll_token().is_err());
}

#[test]
fn secrets_do_not_print() {
    let secret = Secret::new("very-secret");
    assert!(!format!("{secret:?}").contains("very-secret"));
    let credentials = Credentials {
        node_id: "a-00000".into(),
        node_token: secret,
    };
    assert!(!format!("{credentials:?}").contains("very-secret"));
}

#[test]
fn real_fs_write_like_keeps_the_mode_of_the_original() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("telemt.toml");
    std::fs::write(&file, b"old").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();

    RealFs.write_like(&file, b"new", &file).unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), b"new");
    assert_eq!(mode(&file), 0o640);
    assert!(!dir.path().join("telemt.toml.tmp").exists());

    // A backup copy takes the rights of the file it is a copy of.
    let backup = dir.path().join("telemt.toml.bak");
    RealFs.write_like(&backup, b"old", &file).unwrap();
    assert_eq!(std::fs::read(&backup).unwrap(), b"old");
    assert_eq!(mode(&backup), 0o640);
}

#[test]
fn real_fs_write_like_needs_the_original() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing");
    let target = dir.path().join("target");
    let error = RealFs.write_like(&target, b"x", &missing).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    assert!(!target.exists());
    assert!(!dir.path().join("target.tmp").exists());
}

#[test]
fn mem_fs_write_like_behaves_like_the_real_one() {
    let fs = MemFs::new();
    let file = Path::new("/etc/x.toml");
    assert!(fs.write_like(file, b"a", file).is_err());
    fs.write_private(file, b"old").unwrap();
    fs.write_like(Path::new("/etc/x.toml.bak"), b"old", file)
        .unwrap();
    fs.write_like(file, b"new", file).unwrap();
    assert_eq!(fs.read(file).unwrap().unwrap(), b"new");
    assert_eq!(
        fs.read(Path::new("/etc/x.toml.bak")).unwrap().unwrap(),
        b"old"
    );
}

#[test]
fn mem_fs_exists_sees_files_and_their_directories() {
    let fs = MemFs::new();
    fs.write_private(Path::new("/opt/mtproxyl/mtproxy/config.toml"), b"x")
        .unwrap();
    for path in [
        "/opt/mtproxyl/mtproxy/config.toml",
        "/opt/mtproxyl/mtproxy",
        "/opt/mtproxyl",
        "/opt",
    ] {
        assert!(fs.exists(Path::new(path)).unwrap(), "{path}");
    }
    for path in [
        "/opt/mtproxy",
        "/opt/mtproxyl-old",
        "/opt/mtproxyl/mtproxy/config",
    ] {
        assert!(!fs.exists(Path::new(path)).unwrap(), "{path}");
    }
}

#[test]
fn real_fs_exists_sees_files_and_directories_and_not_what_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a");
    std::fs::write(&file, b"x").unwrap();
    assert!(RealFs.exists(&file).unwrap());
    assert!(RealFs.exists(dir.path()).unwrap());
    assert!(!RealFs.exists(&dir.path().join("missing")).unwrap());
}
