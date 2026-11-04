include!("../protocol/build_support/minisign_key.rs");

fn main() {
    embed_minisign_key();
    embed_web();
}

/// Copies `web/dist` to `$OUT_DIR/web`, which `assets.rs` embeds. Without a frontend build the
/// copy is an empty directory, so the registrar still builds (tests, clippy and the stand
/// images do not build the frontend) and its pages answer 404.
fn embed_web() {
    use std::fs;
    use std::path::{Path, PathBuf};

    fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            let target = to.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                copy_tree(&entry.path(), &target)?;
            } else {
                fs::copy(entry.path(), target)?;
            }
        }
        Ok(())
    }

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let dist = manifest.join("../../web/dist");
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("web");

    // A path that does not exist counts as always changed, so while `dist` is absent watch its
    // parent: creating `dist` changes the parent's modification time.
    let watched = if dist.exists() {
        dist.clone()
    } else {
        manifest.join("../../web")
    };
    println!("cargo:rerun-if-changed={}", watched.display());
    println!("cargo:rerun-if-changed=build.rs");

    let _ = fs::remove_dir_all(&out);
    if dist.is_dir() {
        copy_tree(&dist, &out).expect("cannot copy web/dist");
    } else {
        fs::create_dir_all(&out).expect("cannot create the empty web directory");
        if std::env::var("PROFILE").is_ok_and(|p| p == "release") {
            println!(
                "cargo:warning=web/dist is missing: this binary serves no web interface. \
                 Run `npm run build` in web/ first."
            );
        }
    }
}
