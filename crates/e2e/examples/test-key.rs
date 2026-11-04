//! Makes the stand's test key in `e2e/keys/` if there is none yet, for building the images
//! without bringing a stand up: `cargo run -p mtpool-e2e --example test-key`.

fn main() -> mtpool_e2e::Result<()> {
    mtpool_e2e::ensure_test_key().map(|_| ())
}
