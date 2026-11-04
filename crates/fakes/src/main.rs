//! The stand-ins for Globalping, cheburcheck, RedFlag and Cloudflare that the end-to-end stand
//! points the registrar and the nodes at.

use std::net::SocketAddr;

use mtpool_fakes::{Fakes, router};

const DEFAULT_LISTEN: &str = "0.0.0.0:9000";

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let listen = std::env::var("MTPOOL_FAKES_LISTEN").unwrap_or_else(|_| DEFAULT_LISTEN.to_owned());
    let listener = match tokio::net::TcpListener::bind(&listen).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("cannot listen on {listen}: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    println!("mtpool-fakes listens on {listen}");
    let app = router(Fakes::default()).into_make_service_with_connect_info::<SocketAddr>();
    if let Err(error) = axum::serve(listener, app).await {
        eprintln!("the server stopped: {error}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
