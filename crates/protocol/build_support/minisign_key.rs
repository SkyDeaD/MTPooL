// The public signing key, read at build time. Included by `build.rs` of the registrar and of the
// agent, which both embed it: `MTPOOL_MINISIGN_PUB_FILE` names the `.pub` file (a relative path
// is relative to the workspace root; the default is `deploy/minisign.pub`). A release build
// without the file fails; a debug build gets an empty key, and what needs the key says so.

fn embed_minisign_key() {
    use std::path::PathBuf;

    const VAR: &str = "MTPOOL_MINISIGN_PUB_FILE";
    println!("cargo:rerun-if-env-changed={VAR}");
    println!("cargo:rerun-if-env-changed=PROFILE");

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let root = manifest.join("../..");
    let file = match std::env::var(VAR).ok().filter(|v| !v.is_empty()) {
        Some(path) => root.join(path),
        None => root.join("deploy/minisign.pub"),
    };
    println!("cargo:rerun-if-changed={}", file.display());

    let key = match std::fs::read_to_string(&file) {
        Ok(text) => {
            // The file is a comment line and the key line; the key is 42 bytes in base64.
            let line = text
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty() && !l.starts_with("untrusted comment:"))
                .unwrap_or("");
            assert!(
                line.len() == 56
                    && line
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'='),
                "{VAR}: {} does not hold a minisign public key",
                file.display()
            );
            line.to_owned()
        }
        Err(error) => {
            assert!(
                std::env::var("PROFILE").as_deref() != Ok("release"),
                "a release build needs the public signing key: set {VAR} to a minisign .pub \
                 file (default deploy/minisign.pub, not found: {error})"
            );
            String::new()
        }
    };
    println!("cargo:rustc-env=MTPOOL_MINISIGN_PUB={key}");
}
