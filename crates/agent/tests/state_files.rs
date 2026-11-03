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
