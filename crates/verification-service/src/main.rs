#![deny(warnings)]
//! Primora verification service binary: reads configuration from the
//! environment, initializes every dependency, and starts the Axum server.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use anomaly_engine::AnomalyEngine;
use common::{AnomalyEvent, Chain, NodeId, NodeSite};
use mint_ceiling::MintCeilingCalculator;
use node_coordinator::{GrpcNodeClient, NodeCoordinator, ATTESTATION_NODE_COUNT};
use onchain_client::{OnchainClient, OracleSubmitter, StakingReader};
use postgres_store::PostgresStore;
use rate_limiter::RateLimiter;
use session_manager::SessionStore;
use tokio::sync::RwLock;
use verification_service::{serve, AppState};

const DEFAULT_NODE_ENDPOINT: &str = "http://localhost:50051";
const DEFAULT_NODE_API_KEY: &str = "dev-api-key";
const DEFAULT_LOG_LEVEL: &str = "info";
const DEFAULT_MINT_CEILING_ACTIVE_USERS: &str = "1000";
const DEFAULT_MINT_CEILING_AVG_DAILY_PRM_PER_USER: &str = "500";
const ANOMALY_CHANNEL_CAPACITY: usize = 1024;

/// Service configuration assembled from environment variables.
struct Config {
    database_url: String,
    redis_url: String,
    bind_addr: String,
    chain_id: u64,
    rpc_url: String,
    signing_key_hex: String,
    node_endpoints: Vec<(String, String)>,
    node_api_key: String,
    mint_ceiling_active_users: u64,
    mint_ceiling_avg_daily_prm_per_user: u64,
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

/// Parses the comma-separated `NODE_ENDPOINTS` list into `(node_id, endpoint)`
/// pairs. Each entry is either `node_id=endpoint` or a bare `endpoint` (in which
/// case the endpoint also serves as the node id). Whitespace is trimmed and empty
/// entries dropped.
fn parse_node_endpoints(raw: &str) -> Vec<(String, String)> {
    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| match entry.split_once('=') {
            Some((id, endpoint)) => (id.trim().to_string(), endpoint.trim().to_string()),
            None => (entry.to_string(), entry.to_string()),
        })
        .collect()
}

/// Parses the `NODE_SIGNERS` map (`node_id=0xADDRESS`, comma-separated) into each
/// node's registered signing address. Entries without a valid `node_id=0x...`
/// pair are logged and skipped rather than failing startup.
fn parse_node_signers(raw: &str) -> HashMap<NodeId, Address> {
    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| match entry.split_once('=') {
            Some((id, addr)) => match Address::from_str(addr.trim()) {
                Ok(address) => Some((NodeId(id.trim().to_string()), address)),
                Err(_) => {
                    tracing::warn!(entry, "invalid NODE_SIGNERS address; skipping");
                    None
                }
            },
            None => {
                tracing::warn!(entry, "NODE_SIGNERS entry not in node_id=address form; skipping");
                None
            }
        })
        .collect()
}

/// Parses the `NODE_SITES` JSON map (node id -> site metadata). An empty or
/// malformed value logs a warning and yields an empty map rather than failing.
fn parse_node_sites(raw: &str) -> HashMap<String, NodeSite> {
    if raw.trim().is_empty() {
        return HashMap::new();
    }
    match serde_json::from_str::<HashMap<String, NodeSite>>(raw) {
        Ok(map) => map,
        Err(e) => {
            tracing::warn!(error = %e, "invalid NODE_SITES JSON, defaulting to empty");
            HashMap::new()
        }
    }
}

/// Builds one [`OracleSubmitter`] per configured chain (Decision 4b).
///
/// `{PREFIX}_RPC_URL` is shared per-chain infrastructure (also used by the 4d
/// staking reader and read-only seed paths), so its presence alone does NOT
/// configure or imply an oracle submitter. A submitter is keyed solely on its
/// two submitter-specific vars; the RPC is then required to actually build it.
///
/// Per-chain config matrix (KEY = `{PREFIX}_ORACLE_SUBMITTER_KEY_HEX`, ADDR =
/// `{PREFIX}_ORACLE_AGGREGATOR_ADDRESS`, RPC = `{PREFIX}_RPC_URL`):
///   - KEY + ADDR + RPC -> submitter built.
///   - neither KEY nor ADDR -> no submitter (skip), regardless of RPC. RPC alone
///     is valid: the chain may be staking-only or read-only.
///   - exactly one of KEY/ADDR -> fatal: genuine partial oracle config.
///   - KEY + ADDR but no RPC -> fatal: submitter has no endpoint to submit to.
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
        match (key, addr) {
            (Some(key), Some(addr)) => {
                let Some(rpc) = rpc else {
                    tracing::error!(chain = %chain, "startup failed: {prefix}_ORACLE_SUBMITTER_KEY_HEX and {prefix}_ORACLE_AGGREGATOR_ADDRESS set but {prefix}_RPC_URL missing");
                    std::process::exit(1);
                };
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
            (None, None) => {}
            _ => {
                tracing::error!(chain = %chain, "startup failed: partial oracle submitter config (need both {prefix}_ORACLE_SUBMITTER_KEY_HEX and {prefix}_ORACLE_AGGREGATOR_ADDRESS, or neither)");
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

/// Builds one [`StakingReader`] per configured chain for the combined boost
/// (Decision 4d). For each chain, the staking address is read alongside the
/// chain's `{PREFIX}_RPC_URL` (reused from 4b). A chain is configured only when
/// both its RPC URL and staking address are present; otherwise it is skipped
/// with an info log. Staking boost is optional, so partial config is not fatal.
async fn build_staking_readers() -> Vec<(Chain, Arc<StakingReader>)> {
    let mut readers: Vec<(Chain, Arc<StakingReader>)> = Vec::new();
    let mut configured: Vec<Chain> = Vec::new();
    for chain in Chain::all() {
        let prefix = match chain {
            Chain::Ethereum => "ETHEREUM",
            Chain::Polygon => "POLYGON",
        };
        let rpc = std::env::var(format!("{prefix}_RPC_URL")).ok();
        let addr = std::env::var(format!("{prefix}_STAKING_ADDRESS")).ok();
        match (rpc, addr) {
            (Some(rpc), Some(addr)) => {
                let address = match Address::from_str(&addr) {
                    Ok(address) => address,
                    Err(e) => {
                        tracing::warn!(error = %e, chain = %chain, "invalid {prefix}_STAKING_ADDRESS, skipping staking boost on this chain");
                        continue;
                    }
                };
                match StakingReader::new(&rpc, address).await {
                    Ok(reader) => {
                        readers.push((chain, Arc::new(reader)));
                        configured.push(chain);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, chain = %chain, "staking reader init failed, skipping staking boost on this chain");
                    }
                }
            }
            _ => {
                tracing::info!(chain = %chain, "staking reader not configured (need both {prefix}_RPC_URL and {prefix}_STAKING_ADDRESS)");
            }
        }
    }
    if configured.is_empty() {
        tracing::info!("no staking readers configured, staking boost disabled");
    } else {
        tracing::info!(chains = ?configured, "staking readers configured");
    }
    readers
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
    let mint_ceiling_active_users = optional(
        "MINT_CEILING_ACTIVE_USERS",
        DEFAULT_MINT_CEILING_ACTIVE_USERS,
    )
    .parse::<u64>()
    .map_err(|_| "MINT_CEILING_ACTIVE_USERS must be a u64".to_string())?;
    let mint_ceiling_avg_daily_prm_per_user = optional(
        "MINT_CEILING_AVG_DAILY_PRM_PER_USER",
        DEFAULT_MINT_CEILING_AVG_DAILY_PRM_PER_USER,
    )
    .parse::<u64>()
    .map_err(|_| "MINT_CEILING_AVG_DAILY_PRM_PER_USER must be a u64".to_string())?;

    Ok(Config {
        database_url,
        redis_url,
        bind_addr,
        chain_id,
        rpc_url,
        signing_key_hex,
        node_endpoints,
        node_api_key,
        mint_ceiling_active_users,
        mint_ceiling_avg_daily_prm_per_user,
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
    let staking_readers = build_staking_readers().await;

    let mut node_ids: Vec<NodeId> = Vec::new();
    let mut node_clients: HashMap<NodeId, Arc<GrpcNodeClient>> = HashMap::new();
    for (id, endpoint) in &config.node_endpoints {
        let client = match GrpcNodeClient::new(endpoint.clone(), config.node_api_key.clone()).await {
            Ok(client) => client,
            Err(e) => {
                tracing::error!(error = %e, endpoint = %endpoint, "startup failed: node client");
                std::process::exit(1);
            }
        };
        let node_id = NodeId(id.clone());
        node_ids.push(node_id.clone());
        node_clients.insert(node_id, Arc::new(client));
    }

    if node_clients.is_empty() {
        tracing::warn!("no node endpoints configured; using the default single node");
        let client = match GrpcNodeClient::new(
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
        };
        let node_id = NodeId(DEFAULT_NODE_ENDPOINT.to_string());
        node_ids.push(node_id.clone());
        node_clients.insert(node_id, Arc::new(client));
    }
    if node_ids.len() < ATTESTATION_NODE_COUNT {
        tracing::warn!(
            configured = node_ids.len(),
            required = ATTESTATION_NODE_COUNT,
            "fewer attestation nodes than the 3-of-4 quorum needs; attestation will fail to reach quorum until at least {ATTESTATION_NODE_COUNT} nodes are configured"
        );
    }
    let node_signers = parse_node_signers(&optional("NODE_SIGNERS", ""));
    if node_signers.is_empty() {
        tracing::warn!("NODE_SIGNERS is empty; no attestation signature can be verified, so attestation cannot reach quorum");
    }
    let node_coordinator = NodeCoordinator::new(node_clients, node_ids, node_signers);

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
        staking_readers,
        twap_sessions: Arc::new(RwLock::new(HashMap::new())),
        node_sites: Arc::new(parse_node_sites(&optional("NODE_SITES", ""))),
        mint_ceiling_active_users: config.mint_ceiling_active_users,
        mint_ceiling_avg_daily_prm_per_user: config.mint_ceiling_avg_daily_prm_per_user,
    };

    let bind_addr = config.bind_addr;
    tracing::info!(addr = %bind_addr, "primora verification service starting");
    if let Err(e) = serve(state, &bind_addr).await {
        tracing::error!(error = %e, "startup failed: server");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_node_sites_valid() {
        let raw = r#"{"node-a":{"code":"JHB","city":"Johannesburg","country":"ZA"}}"#;
        let map = parse_node_sites(raw);
        let site = map.get("node-a").expect("node-a present");
        assert_eq!(site.code, "JHB");
        assert_eq!(site.city, "Johannesburg");
        assert_eq!(site.country, "ZA");
    }

    #[test]
    fn parse_node_sites_empty_or_malformed_is_empty() {
        assert!(parse_node_sites("").is_empty());
        assert!(parse_node_sites("   ").is_empty());
        assert!(parse_node_sites("{not valid json").is_empty());
    }
}
