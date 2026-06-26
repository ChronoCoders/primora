#![deny(warnings)]
//! Primora verification service binary: reads configuration from the
//! environment, initializes every dependency, and starts the Axum server.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use anomaly_engine::AnomalyEngine;
use common::{AnomalyEvent, Chain, NodeId};
use mint_ceiling::MintCeilingCalculator;
use node_coordinator::{GrpcNodeClient, NodeCoordinator};
use onchain_client::{OnchainClient, OracleSubmitter};
use postgres_store::PostgresStore;
use rate_limiter::RateLimiter;
use session_manager::SessionStore;
use tokio::sync::RwLock;
use verification_service::{serve, AppState};

const DEFAULT_NODE_ENDPOINT: &str = "http://localhost:50051";
const DEFAULT_NODE_API_KEY: &str = "dev-api-key";
const DEFAULT_LOG_LEVEL: &str = "info";
const ANOMALY_CHANNEL_CAPACITY: usize = 1024;

/// Service configuration assembled from environment variables.
struct Config {
    database_url: String,
    redis_url: String,
    bind_addr: String,
    chain_id: u64,
    rpc_url: String,
    signing_key_hex: String,
    node_endpoints: Vec<String>,
    node_api_key: String,
}

/// Reads a required environment variable, returning a descriptive error when it
/// is absent.
fn require(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("missing required env var {name}"))
}

/// Reads an optional environment variable, falling back to `default`.
fn optional(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

/// Parses the comma-separated `NODE_ENDPOINTS` list, trimming whitespace and
/// dropping empty entries.
fn parse_node_endpoints(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect()
}

/// Builds one [`OracleSubmitter`] per configured chain (Decision 4b). For each
/// chain, the RPC, submitter key, and aggregator address env vars are read as a
/// triple: all three present builds a submitter; all three absent skips that
/// chain; a partial triple is a misconfiguration and exits the process.
///
/// `ETHEREUM_RPC_URL` (submission) is independent of `RPC_URL` (the canonical
/// oracle read); the two may point at the same endpoint but are configured
/// separately.
async fn build_oracle_submitters() -> Vec<(Chain, Arc<OracleSubmitter>)> {
    let mut submitters: Vec<(Chain, Arc<OracleSubmitter>)> = Vec::new();
    let mut configured: Vec<Chain> = Vec::new();
    for chain in Chain::all() {
        let prefix = match chain {
            Chain::Ethereum => "ETHEREUM",
            Chain::Polygon => "POLYGON",
        };
        let rpc = std::env::var(format!("{prefix}_RPC_URL")).ok();
        let key = std::env::var(format!("{prefix}_ORACLE_SUBMITTER_KEY_HEX")).ok();
        let addr = std::env::var(format!("{prefix}_ORACLE_AGGREGATOR_ADDRESS")).ok();
        match (rpc, key, addr) {
            (Some(rpc), Some(key), Some(addr)) => {
                let address = match Address::from_str(&addr) {
                    Ok(address) => address,
                    Err(e) => {
                        tracing::error!(error = %e, chain = %chain, "startup failed: {prefix}_ORACLE_AGGREGATOR_ADDRESS parse");
                        std::process::exit(1);
                    }
                };
                let signer = match PrivateKeySigner::from_str(&key) {
                    Ok(signer) => signer,
                    Err(e) => {
                        tracing::error!(error = %e, chain = %chain, "startup failed: {prefix}_ORACLE_SUBMITTER_KEY_HEX parse");
                        std::process::exit(1);
                    }
                };
                match OracleSubmitter::new(&rpc, signer, address).await {
                    Ok(submitter) => {
                        submitters.push((chain, Arc::new(submitter)));
                        configured.push(chain);
                    }
                    Err(e) => {
                        tracing::error!(error = %e, chain = %chain, "startup failed: oracle submitter init");
                        std::process::exit(1);
                    }
                }
            }
            (None, None, None) => {}
            _ => {
                tracing::error!(chain = %chain, "startup failed: partial oracle submitter config (need all of {prefix}_RPC_URL, {prefix}_ORACLE_SUBMITTER_KEY_HEX, {prefix}_ORACLE_AGGREGATOR_ADDRESS, or none)");
                std::process::exit(1);
            }
        }
    }
    if configured.is_empty() {
        tracing::info!("no oracle submitters configured, TWAP submission disabled");
    } else {
        tracing::info!(chains = ?configured, "oracle submitters configured");
    }
    submitters
}

/// Loads and validates all configuration from the environment.
fn load_config() -> Result<Config, String> {
    let database_url = require("DATABASE_URL")?;
    let redis_url = require("REDIS_URL")?;
    let bind_addr = require("BIND_ADDR")?;
    let chain_id = require("CHAIN_ID")?
        .parse::<u64>()
        .map_err(|_| "CHAIN_ID must be a u64".to_string())?;
    let rpc_url = require("RPC_URL")?;
    let signing_key_hex = require("SIGNING_KEY_HEX")?;
    let node_endpoints =
        parse_node_endpoints(&optional("NODE_ENDPOINTS", DEFAULT_NODE_ENDPOINT));
    let node_api_key = optional("NODE_API_KEY", DEFAULT_NODE_API_KEY);

    Ok(Config {
        database_url,
        redis_url,
        bind_addr,
        chain_id,
        rpc_url,
        signing_key_hex,
        node_endpoints,
        node_api_key,
    })
}

/// Initializes the tracing subscriber from the `LOG_LEVEL` environment variable.
fn init_tracing() {
    let log_level = optional("LOG_LEVEL", DEFAULT_LOG_LEVEL);
    let filter = tracing_subscriber::EnvFilter::try_new(&log_level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_LOG_LEVEL));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[tokio::main]
async fn main() {
    init_tracing();

    let config = match load_config() {
        Ok(config) => config,
        Err(e) => {
            tracing::error!(error = %e, "startup failed: configuration");
            std::process::exit(1);
        }
    };

    if let Err(e) = metrics::register_all() {
        tracing::error!(error = %e, "startup failed: metrics registration");
        std::process::exit(1);
    }

    let postgres_store = match PostgresStore::new(&config.database_url).await {
        Ok(store) => store,
        Err(e) => {
            tracing::error!(error = %e, "startup failed: postgres connect");
            std::process::exit(1);
        }
    };
    if let Err(e) = postgres_store.run_migrations().await {
        tracing::error!(error = %e, "startup failed: postgres migrations");
        std::process::exit(1);
    }

    let session_store = match SessionStore::new(&config.redis_url).await {
        Ok(store) => store,
        Err(e) => {
            tracing::error!(error = %e, "startup failed: redis session store connect");
            std::process::exit(1);
        }
    };

    let rate_limiter = match RateLimiter::new(&config.redis_url).await {
        Ok(limiter) => limiter,
        Err(e) => {
            tracing::error!(error = %e, "startup failed: redis rate limiter connect");
            std::process::exit(1);
        }
    };

    let onchain_client = match OnchainClient::new(&config.rpc_url, config.chain_id).await {
        Ok(client) => client,
        Err(e) => {
            tracing::error!(error = %e, "startup failed: onchain client");
            std::process::exit(1);
        }
    };

    let oracle_reader = oracle_reader::OracleReader::new(
        &config.rpc_url,
        oracle_reader::feeds_from_env_or_default(),
        oracle_reader::default_pyth_feeds(),
    )
    .await
    .unwrap_or_else(|e| {
        tracing::error!(error = %e, "startup failed: oracle reader init");
        std::process::exit(1);
    });

    let signer = match PrivateKeySigner::from_str(&config.signing_key_hex) {
        Ok(signer) => signer,
        Err(e) => {
            tracing::error!(error = %e, "startup failed: signing key");
            std::process::exit(1);
        }
    };

    let oracle_submitters = build_oracle_submitters().await;

    let mut node_ids: Vec<NodeId> = Vec::new();
    let mut first_client: Option<GrpcNodeClient> = None;
    for endpoint in &config.node_endpoints {
        let client = match GrpcNodeClient::new(endpoint.clone(), config.node_api_key.clone()).await {
            Ok(client) => client,
            Err(e) => {
                tracing::error!(error = %e, endpoint = %endpoint, "startup failed: node client");
                std::process::exit(1);
            }
        };
        node_ids.push(NodeId(endpoint.clone()));
        if first_client.is_none() {
            first_client = Some(client);
        }
    }

    let grpc_client = match first_client {
        Some(client) => client,
        None => {
            tracing::warn!("no node endpoints configured; using empty node list");
            match GrpcNodeClient::new(
                DEFAULT_NODE_ENDPOINT.to_string(),
                config.node_api_key.clone(),
            )
            .await
            {
                Ok(client) => client,
                Err(e) => {
                    tracing::error!(error = %e, "startup failed: fallback node client");
                    std::process::exit(1);
                }
            }
        }
    };
    let node_coordinator = NodeCoordinator::new(Arc::new(grpc_client), node_ids);

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AnomalyEvent>(ANOMALY_CHANNEL_CAPACITY);
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            tracing::info!(
                session_id = %event.session_id.0,
                level = ?event.level,
                "anomaly event received"
            );
        }
    });
    let anomaly_engine = AnomalyEngine::new(tx);

    let state = AppState {
        session_manager: Arc::new(session_store),
        rate_limiter: Arc::new(rate_limiter),
        anomaly_engine: Arc::new(anomaly_engine),
        mint_ceiling: Arc::new(MintCeilingCalculator::new()),
        onchain_client: Arc::new(onchain_client),
        postgres_store: Arc::new(postgres_store),
        node_coordinator: Arc::new(node_coordinator),
        signing_key: Arc::new(signer),
        oracle_reader: Arc::new(oracle_reader),
        oracle_submitters,
        twap_sessions: Arc::new(RwLock::new(HashMap::new())),
    };

    let bind_addr = config.bind_addr;
    tracing::info!(addr = %bind_addr, "primora verification service starting");
    if let Err(e) = serve(state, &bind_addr).await {
        tracing::error!(error = %e, "startup failed: server");
        std::process::exit(1);
    }
}
