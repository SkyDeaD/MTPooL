//! The engine is picked by what the host has under `/opt`.

use std::path::Path;
use std::sync::Arc;

use mtpool_agent::engine::http::testing::ScriptedHttp;
use mtpool_agent::engine::select::select_engine;
use mtpool_agent::engine::{Engine, EngineKind};
use mtpool_agent::host::testing::ScriptedRunner;
use mtpool_agent::state_files::{Fs, MemFs};

async fn kind_with(dirs: &[&str]) -> EngineKind {
    let fs = Arc::new(MemFs::new());
    for dir in dirs {
        fs.write_private(&Path::new(dir).join("marker"), b"x")
            .unwrap();
    }
    let engine = select_engine(
        "/state".into(),
        Arc::new(ScriptedRunner::new()),
        fs,
        Arc::new(ScriptedHttp::new()),
    )
    .await;
    engine.kind()
}

#[tokio::test]
async fn the_engine_follows_the_host() {
    assert_eq!(kind_with(&[]).await, EngineKind::Telemt);
    assert_eq!(kind_with(&["/opt/mtpr-simple"]).await, EngineKind::Meko);
    assert_eq!(kind_with(&["/opt/mtproxyl"]).await, EngineKind::MtProxyL);
    assert_eq!(
        kind_with(&["/opt/mtproxyl", "/opt/mtpr-simple"]).await,
        EngineKind::Meko
    );
}
