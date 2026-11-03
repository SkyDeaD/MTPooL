use std::process::ExitCode;

use clap::Parser;
use mimalloc::MiMalloc;
use mtpool_registrar::cli::{self, Cli};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    cli::init_logging();
    cli::run(cli).await
}
