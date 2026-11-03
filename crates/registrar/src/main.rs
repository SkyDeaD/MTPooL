use std::process::ExitCode;

use clap::Parser;
use mimalloc::MiMalloc;
use mtpool_registrar::cli::{self, Cli};
use mtpool_registrar::config::Config;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    cli::init_logging(&config.log_filter);
    cli::run(cli, config).await
}
