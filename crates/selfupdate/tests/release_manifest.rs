//! `scripts/make-manifest.sh` writes the manifest that the release workflow signs. These tests
//! run the real script on fake artifacts and feed what it wrote to the real parser, so the two
//! cannot drift apart.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use mtpool_selfupdate::manifest::{
    AGENT_COMPONENT, KEY_COMPONENT, REGISTRAR_COMPONENT, rotated_key, sha256_hex,
    verify_manifest_with,
};

const TARGETS: [&str; 2] = ["x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"];

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/make-manifest.sh")
}

fn run(args: &[&str]) -> Output {
    Command::new("bash")
        .arg(script())
        .args(args)
        .output()
        .expect("bash runs")
}

/// A directory with a fake build of both components for both targets.
fn fake_release() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for component in [REGISTRAR_COMPONENT, AGENT_COMPONENT] {
        for target in TARGETS {
            let name = format!("{component}-{target}");
            std::fs::write(dir.path().join(&name), format!("fake build {name}\n")).unwrap();
        }
    }
    dir
}

fn sign(pair: &minisign::KeyPair, data: &[u8]) -> String {
    minisign::sign(Some(&pair.pk), &pair.sk, data, None, None)
        .unwrap()
        .into_string()
}

fn pair() -> minisign::KeyPair {
    minisign::KeyPair::generate_unencrypted_keypair().unwrap()
}

#[test]
fn a_normal_release_is_accepted_by_the_parser() {
    let dir = fake_release();
    let out = run(&["0.3.1", dir.path().to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let bytes = std::fs::read(dir.path().join("manifest.json")).unwrap();
    let pair = pair();
    let manifest =
        verify_manifest_with(&pair.pk.to_base64(), &bytes, &sign(&pair, &bytes)).unwrap();

    assert_eq!(manifest.version.to_string(), "0.3.1");
    assert!(
        manifest.key_artifact().is_none(),
        "a normal release announces no key"
    );
    assert_eq!(manifest.artifacts.len(), 4);
    for component in [REGISTRAR_COMPONENT, AGENT_COMPONENT] {
        for target in TARGETS {
            let artifact = manifest
                .artifact(component, target)
                .unwrap_or_else(|| panic!("no {component} for {target}"));
            assert_eq!(artifact.name, format!("{component}-{target}"));
            let file = std::fs::File::open(dir.path().join(&artifact.name)).unwrap();
            assert_eq!(artifact.sha256, sha256_hex(file).unwrap());
        }
    }
}

#[test]
fn a_pre_release_version_is_accepted() {
    let dir = fake_release();
    let out = run(&["0.0.1-rc.1", dir.path().to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let bytes = std::fs::read(dir.path().join("manifest.json")).unwrap();
    let pair = pair();
    let manifest =
        verify_manifest_with(&pair.pk.to_base64(), &bytes, &sign(&pair, &bytes)).unwrap();
    assert_eq!(manifest.version.to_string(), "0.0.1-rc.1");
}

#[test]
fn a_key_rotation_announces_the_new_key() {
    let dir = fake_release();
    let old = pair();
    let new = pair();
    let key_file = new.pk.to_box().unwrap().into_string();
    std::fs::write(dir.path().join("minisign.pub"), &key_file).unwrap();

    let out = run(&[
        "1.0.0",
        dir.path().to_str().unwrap(),
        "--announce-key",
        "minisign.pub",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let bytes = std::fs::read(dir.path().join("manifest.json")).unwrap();
    // The old key signs the release that announces the new one.
    let manifest = verify_manifest_with(&old.pk.to_base64(), &bytes, &sign(&old, &bytes)).unwrap();
    let announced = manifest.key_artifact().expect("the key is announced");
    assert_eq!(announced.component, KEY_COMPONENT);
    assert_eq!(announced.name, "minisign.pub");
    assert_eq!(manifest.artifacts.len(), 5);
    let accepted = rotated_key(&manifest, key_file.as_bytes()).unwrap();
    assert_eq!(accepted, new.pk.to_base64());
}

#[test]
fn bad_input_is_refused_and_no_manifest_is_left() {
    let dir = fake_release();
    let path = dir.path().to_str().unwrap();
    for version in [
        "",
        "1.2",
        "v1.2.3",
        "01.2.3",
        "1.2.3+build",
        "1.2.3-",
        "1.2.3-01",
        "1.2.3 ",
    ] {
        let out = run(&[version, path]);
        assert!(!out.status.success(), "version {version:?} must be refused");
    }
    // A missing build.
    std::fs::remove_file(dir.path().join("mtpool-agent-aarch64-unknown-linux-musl")).unwrap();
    assert!(!run(&["1.2.3", path]).status.success());
    // A key file that is not in the directory, or whose name is not a plain name.
    let dir = fake_release();
    let path = dir.path().to_str().unwrap();
    assert!(
        !run(&["1.2.3", path, "--announce-key", "minisign.pub"])
            .status
            .success()
    );
    assert!(
        !run(&["1.2.3", path, "--announce-key", "../x"])
            .status
            .success()
    );
    assert!(!dir.path().join("manifest.json").exists());
}
