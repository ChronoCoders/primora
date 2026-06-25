#![deny(warnings)]
//! Primora node server binary: reads configuration from the environment and
//! serves the gRPC `NodeService` until shutdown.

use std::net::SocketAddr;

const DEFAULT_NODE_ID: &str = "node-unknown";
const DEFAULT_LOG_LEVEL: &str = "info";

/// Initializes the tracing subscriber from the `LOG_LEVEL` environment variable.
fn init_tracing() {
    let log_level = std::env::var("LOG_LEVEL").unwrap_or_else(|_| DEFAULT_LOG_LEVEL.to_string());
    let filter = tracing_subscriber::EnvFilter::try_new(&log_level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_LOG_LEVEL));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[tokio::main]
async fn main() {
    init_tracing();

    let bind_addr = match std::env::var("BIND_ADDR") {
        Ok(addr) => addr,
        Err(_) => {
            tracing::error!("startup failed: missing required env var BIND_ADDR");
            std::process::exit(1);
        }
    };
    let api_key = match std::env::var("NODE_API_KEY") {
        Ok(key) => key,
        Err(_) => {
            tracing::error!("startup failed: missing required env var NODE_API_KEY");
            std::process::exit(1);
        }
    };
    let node_id = std::env::var("NODE_ID").unwrap_or_else(|_| DEFAULT_NODE_ID.to_string());

    let addr: SocketAddr = match bind_addr.parse() {
        Ok(addr) => addr,
        Err(e) => {
            tracing::error!(error = %e, "startup failed: invalid BIND_ADDR");
            std::process::exit(1);
        }
    };

    let server = match node_server::build_server(api_key) {
        Ok(server) => server,
        Err(e) => {
            tracing::error!(error = %e, "startup failed: randomx verifier init");
            std::process::exit(1);
        }
    };
    tracing::info!(addr = %bind_addr, node_id = %node_id, "primora node starting");
    if let Err(e) = tonic::transport::Server::builder()
        .add_service(server)
        .serve(addr)
        .await
    {
        tracing::error!(error = %e, "startup failed: node server");
        std::process::exit(1);
    }
}
