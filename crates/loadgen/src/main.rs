use clap::Parser;
use mtpool_loadgen::args::Args;

#[tokio::main]
async fn main() {
    let args = Args::parse();
    match mtpool_loadgen::run::run(args).await {
        Ok(()) => {}
        Err(message) => {
            eprintln!("mtpool-loadgen: {message}");
            std::process::exit(1);
        }
    }
}
