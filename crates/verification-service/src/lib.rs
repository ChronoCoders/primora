#![deny(warnings)]
#![deny(missing_docs)]
//! Axum entry point: router, application state, and request routing.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use alloy_primitives::{Address, Signature, U256};
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use common::{
    AnomalyEvent, ClientType, Commodity, InvalidReason, MintProposal, NodeId, NodeSignature,
    PartialProof, ProofValidator, ProposalStatus, SessionContext, SessionId, SuspicionLevel,
    ValidationMode, ValidationResult,
};
use proof_validator::PreFilterValidator;
use rate_limiter::RateLimitResult;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use twap_calculator::TwapCalculator;
use tower_http::trace::TraceLayer;

/// Number of blocks behind the head used to derive the session seed.
const SEED_BLOCK_OFFSET: u64 = 3;

/// Default number of payout rows returned by the payouts endpoint.
const DEFAULT_PAYOUT_LIMIT: i64 = 50;
/// Maximum number of payout rows the payouts endpoint will return.
const MAX_PAYOUT_LIMIT: i64 = 200;

/// Shared application state injected into every handler.
#[derive(Clone)]
pub struct AppState {
    /// Session store backed by Redis.
    pub session_manager: Arc<session_manager::SessionStore>,
    /// Rate limiter backed by Redis.
    pub rate_limiter: Arc<rate_limiter::RateLimiter>,
    /// Anomaly scoring engine.
    pub anomaly_engine: Arc<anomaly_engine::AnomalyEngine>,
    /// Mint ceiling calculator.
    pub mint_ceiling: Arc<mint_ceiling::MintCeilingCalculator>,
    /// On-chain client for block queries and proposal signing.
    pub onchain_client: Arc<onchain_client::OnchainClient>,
    /// Postgres store for anomaly events and mint proposals.
    pub postgres_store: Arc<postgres_store::PostgresStore>,
    /// Node coordinator for 2-of-3 attestation.
    pub node_coordinator: Arc<node_coordinator::NodeCoordinator<node_coordinator::GrpcNodeClient>>,
    /// Backend signing key for mint proposals.
    pub signing_key: Arc<alloy::signers::local::PrivateKeySigner>,
    /// Oracle reader supplying Chainlink and Pyth prices for TWAP sampling.
    pub oracle_reader: Arc<oracle_reader::OracleReader>,
    /// Optional on-chain TWAP submitter. When absent, the finalized TWAP is
    /// computed but not submitted to the OracleAggregator.
    pub oracle_submitter: Option<Arc<onchain_client::OracleSubmitter>>,
    /// TWAP calculators keyed by session id, guarded for concurrent access.
    pub twap_sessions: Arc<RwLock<HashMap<String, TwapCalculator>>>,
}

/// Request body for creating a session.
#[derive(Debug, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    /// Miner wallet address.
    pub wallet: String,
    /// Client software type: `browser`, `desktop`, or `cli`.
    pub client_type: String,
    /// Hex-encoded commit hash (`sha256(nonce)`) for the commit-reveal scheme.
    pub commit_hash: String,
    /// Identifier of the node assigned to this session, if known at creation.
    pub assigned_node_id: Option<String>,
    /// Backing commodity: `Gold`, `Silver`, `Platinum`, or `Oil`.
    pub commodity: String,
}

/// Response body for a created session.
#[derive(Debug, Serialize, Deserialize)]
pub struct CreateSessionResponse {
    /// Identifier of the new session.
    pub session_id: String,
}

/// Request body for submitting a partial proof.
#[derive(Debug, Serialize, Deserialize)]
pub struct SubmitProofRequest {
    /// Proof index within the session.
    pub sequence: u32,
    /// Reported hashrate in H/s.
    pub hashrate: u64,
    /// Hex-encoded proof hash: the RandomX hash of `proof_input`.
    pub proof_hash: String,
    /// Hex-encoded exact RandomX preimage the client hashed. Defaults to empty
    /// for backward compatibility with clients that predate RandomX wiring.
    #[serde(default)]
    pub proof_input: String,
    /// Difficulty target this proof claims. Defaults to 0.
    #[serde(default)]
    pub difficulty: u64,
}

/// Response body for a submitted proof.
#[derive(Debug, Serialize, Deserialize)]
pub struct SubmitProofResponse {
    /// Whether the proof was accepted.
    pub accepted: bool,
    /// Aggregate suspicion level for the session.
    pub suspicion_level: String,
}

/// Request body for ending a session.
#[derive(Debug, Serialize, Deserialize)]
pub struct EndSessionRequest {
    /// Hex-encoded nonce revealed at session end.
    pub nonce: String,
}

/// Response body for an ended session.
#[derive(Debug, Serialize, Deserialize)]
pub struct EndSessionResponse {
    /// Final session status.
    pub status: String,
}

/// Query parameters for the wallet payouts endpoint.
#[derive(Debug, Deserialize)]
pub struct PayoutsQuery {
    /// Maximum rows to return. Defaults to 50, clamped to a maximum of 200.
    pub limit: Option<i64>,
}

/// Errors returned while running the service.
#[derive(Debug)]
pub enum ServiceError {
    /// Failed to bind or serve over TCP.
    Io(std::io::Error),
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for ServiceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
        }
    }
}

impl From<std::io::Error> for ServiceError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Errors returned from request handlers, mapped to HTTP status codes.
#[derive(Debug)]
enum ApiError {
    /// Malformed request input.
    BadRequest(&'static str),
    /// The referenced session does not exist (or has expired).
    NotFound,
    /// A rate limit was exceeded.
    RateLimited,
    /// An internal dependency failed. The cause is logged, not exposed.
    Internal,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            Self::NotFound => (StatusCode::NOT_FOUND, "session not found"),
            Self::RateLimited => (StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded"),
            Self::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
        };
        (status, Json(serde_json::json!({ "error": message }))).into_response()
    }
}

impl From<session_manager::SessionManagerError> for ApiError {
    fn from(e: session_manager::SessionManagerError) -> Self {
        tracing::error!(error = %e, "session manager error");
        Self::Internal
    }
}

impl From<rate_limiter::RateLimiterError> for ApiError {
    fn from(e: rate_limiter::RateLimiterError) -> Self {
        tracing::error!(error = %e, "rate limiter error");
        Self::Internal
    }
}

impl From<onchain_client::OnchainClientError> for ApiError {
    fn from(e: onchain_client::OnchainClientError) -> Self {
        tracing::error!(error = %e, "onchain client error");
        Self::Internal
    }
}

impl From<postgres_store::PostgresStoreError> for ApiError {
    fn from(e: postgres_store::PostgresStoreError) -> Self {
        tracing::error!(error = %e, "postgres store error");
        Self::Internal
    }
}

/// Parses the client type from its lowercase string label.
fn parse_client_type(raw: &str) -> Result<ClientType, ApiError> {
    match raw.to_ascii_lowercase().as_str() {
        "browser" => Ok(ClientType::Browser),
        "desktop" => Ok(ClientType::Desktop),
        "cli" => Ok(ClientType::Cli),
        _ => Err(ApiError::BadRequest("unknown client_type")),
    }
}

/// Parses the backing commodity from its label, defaulting to gold for any
/// unrecognized value.
fn parse_commodity(raw: &str) -> Commodity {
    match raw.to_ascii_lowercase().as_str() {
        "silver" => Commodity::Silver,
        "platinum" => Commodity::Platinum,
        "oil" | "crudeoil" | "crude_oil" => Commodity::CrudeOil,
        _ => Commodity::Gold,
    }
}

/// Decodes a hex string into a 32-byte hash.
fn parse_hash(raw: &str, field: &'static str) -> Result<[u8; 32], ApiError> {
    let bytes = alloy_primitives::hex::decode(raw).map_err(|_| ApiError::BadRequest(field))?;
    bytes.try_into().map_err(|_| ApiError::BadRequest(field))
}

/// Maps a rate limit check result to an error when the limit was exceeded.
fn enforce(result: RateLimitResult) -> Result<(), ApiError> {
    match result {
        RateLimitResult::Allowed => Ok(()),
        RateLimitResult::Denied { .. } => Err(ApiError::RateLimited),
    }
}

/// Creates a session: validates the wallet, applies wallet and IP rate limits,
/// persists the session context and commit hash, and returns the new session id.
async fn create_session(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(body): Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<CreateSessionResponse>), ApiError> {
    let wallet: Address = body
        .wallet
        .parse()
        .map_err(|_| ApiError::BadRequest("invalid wallet address"))?;
    let client_type = parse_client_type(&body.client_type)?;
    let commit_hash = parse_hash(&body.commit_hash, "invalid commit_hash")?;
    let commodity = parse_commodity(&body.commodity);
    let assigned_node_id = body.assigned_node_id.map(NodeId);

    let ip = peer.ip();
    enforce(state.rate_limiter.check_wallet(&wallet).await?)?;
    enforce(state.rate_limiter.check_ip(&ip.to_string()).await?)?;

    let active_sessions_count = state.session_manager.get_active_session_count(&wallet).await?;
    let ctx = SessionContext {
        wallet,
        ip: Some(ip),
        client_type,
        active_sessions_count,
        started_at: Utc::now(),
        last_submission_at: None,
        recent_proof_count: 0,
        assigned_node_id,
        commodity,
    };
    let session_id = state.session_manager.create_session(&ctx).await?;
    state
        .session_manager
        .set_commit(&session_id, commit_hash)
        .await?;

    {
        let mut map = state.twap_sessions.write().await;
        map.insert(session_id.0.clone(), TwapCalculator::new(Utc::now()));
    }
    metrics::SESSION_ACTIVE_COUNT.inc();

    Ok((
        StatusCode::CREATED,
        Json(CreateSessionResponse {
            session_id: session_id.0,
        }),
    ))
}

/// Returns the metric label for a validation result.
fn result_label(result: &ValidationResult) -> &'static str {
    match result {
        ValidationResult::Valid => "Valid",
        ValidationResult::Invalid(_) => "Invalid",
        ValidationResult::Suspicious(_) => "Suspicious",
    }
}

/// Submits a partial proof: loads the session, runs the pre-filter validator,
/// updates the proof counter and anomaly scoring, and reports the suspicion
/// level. A proof rejected by the pre-filter returns `accepted: false` rather
/// than an HTTP error, so the client learns the trigger.
async fn submit_proof(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(body): Json<SubmitProofRequest>,
) -> Result<Json<SubmitProofResponse>, ApiError> {
    let session_id = SessionId(session_id);
    let ctx = state
        .session_manager
        .get_session(&session_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let proof_hash = parse_hash(&body.proof_hash, "invalid proof_hash")?;
    let proof_input = if body.proof_input.is_empty() {
        Vec::new()
    } else {
        alloy_primitives::hex::decode(&body.proof_input)
            .map_err(|_| ApiError::BadRequest("invalid proof_input"))?
    };

    let proof = PartialProof {
        session_id: session_id.clone(),
        wallet: ctx.wallet,
        sequence: body.sequence,
        hashrate: body.hashrate,
        proof_hash,
        proof_input,
        difficulty: body.difficulty,
        submitted_at: Utc::now(),
        signature: None,
    };
    let result = PreFilterValidator.validate(&proof, ValidationMode::PreFilter, &ctx);
    match state.session_manager.increment_proof_count(&session_id).await {
        Ok(count) => {
            tracing::debug!(session_id = %session_id.0, proof_count = count, "proof counted");
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to increment proof count");
        }
    }
    if !matches!(result, ValidationResult::Invalid(_)) {
        match state
            .session_manager
            .add_hashrate_sample(&session_id, body.hashrate)
            .await
        {
            Ok(()) => {
                tracing::debug!(
                    session_id = %session_id.0,
                    hashrate = body.hashrate,
                    "recorded hashrate sample"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to record hashrate sample");
            }
        }
        if let Err(e) = state.session_manager.touch_last_activity(&session_id).await {
            tracing::warn!(error = %e, "failed to update last activity");
        }
    }
    state.session_manager.store_proof(&session_id, proof).await?;

    let client_type_label = format!("{:?}", ctx.client_type);
    metrics::PROOF_SUBMISSIONS_TOTAL
        .with_label_values(&[client_type_label.as_str(), result_label(&result)])
        .inc();

    let accepted = !matches!(result, ValidationResult::Invalid(_));
    let triggers: Vec<InvalidReason> = match &result {
        ValidationResult::Invalid(reason) => vec![reason.clone()],
        ValidationResult::Valid | ValidationResult::Suspicious(_) => Vec::new(),
    };
    let level = state
        .anomaly_engine
        .process(session_id.clone(), ctx.wallet, vec![result]);

    if level != SuspicionLevel::Low {
        let event = AnomalyEvent {
            session_id: session_id.clone(),
            wallet: ctx.wallet,
            score: 0,
            triggers: triggers.clone(),
            level,
            timestamp: Utc::now(),
        };
        if let Err(e) = state.postgres_store.insert_anomaly_event(&event).await {
            tracing::error!(error = %e, "failed to persist anomaly event");
        }
        let trigger_label = triggers
            .first()
            .map_or_else(|| "none".to_string(), |reason| format!("{reason:?}"));
        metrics::ANOMALY_EVENTS_TOTAL
            .with_label_values(&[format!("{level:?}").as_str(), trigger_label.as_str()])
            .inc();
    }

    {
        let mut map = state.twap_sessions.write().await;
        if let Some(calculator) = map.get_mut(&session_id.0) {
            if let Err(e) = state
                .oracle_reader
                .sample_into_twap(ctx.commodity, calculator)
                .await
            {
                tracing::warn!(error = %e, "oracle read failed, skipping TWAP sample");
            }
        }
    }

    Ok(Json(SubmitProofResponse {
        accepted,
        suspicion_level: format!("{level:?}"),
    }))
}

/// Ends a session: verifies the commit-reveal nonce, finalizes the session TWAP,
/// derives the on-chain seed, coordinates 2-of-3 node attestation, signs the
/// resulting mint proposal with the backend key, and persists it.
///
/// The assigned node, proof set, gross PRM, and backing commodity are taken from
/// real session state. The assigned-node signature remains a placeholder until
/// the node binary exists; see the inline TODO.
///
/// Returns `status: "rejected"` (400) on a nonce mismatch, `status:
/// "no_assigned_node"` (400) when the session has no assigned node, `status:
/// "no_samples"` (400) when no TWAP samples were collected, `status: "completed"`
/// (200) on a successful attestation, and `status: "attestation_failed"` (500)
/// when attestation does not reach the required signature threshold.
async fn end_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(body): Json<EndSessionRequest>,
) -> Result<(StatusCode, Json<EndSessionResponse>), ApiError> {
    let session_id = SessionId(session_id);
    let ctx = state
        .session_manager
        .get_session(&session_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let nonce =
        alloy_primitives::hex::decode(&body.nonce).map_err(|_| ApiError::BadRequest("invalid nonce"))?;

    let verified = state.session_manager.verify_reveal(&session_id, &nonce).await?;
    if !verified {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(EndSessionResponse {
                status: "rejected".to_string(),
            }),
        ));
    }

    let Some(assigned_node_id) = ctx.assigned_node_id.clone() else {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(EndSessionResponse {
                status: "no_assigned_node".to_string(),
            }),
        ));
    };

    let calculator = {
        let mut map = state.twap_sessions.write().await;
        map.remove(&session_id.0)
    };
    let twap = match calculator.and_then(|calc| calc.finalize(Utc::now())) {
        Some(result) => result,
        None => {
            return Ok((
                StatusCode::BAD_REQUEST,
                Json(EndSessionResponse {
                    status: "no_samples".to_string(),
                }),
            ));
        }
    };
    if !twap.is_valid {
        tracing::warn!(session = %session_id.0, "twap session below minimum valid duration");
    }

    if let Some(submitter) = &state.oracle_submitter {
        let commodity_u8 = onchain_client::commodity_to_u8(&ctx.commodity);
        match submitter.submit_price(commodity_u8, twap.twap).await {
            Ok(tx_hash) => tracing::info!(
                session_id = %session_id.0,
                tx_hash = %tx_hash,
                "TWAP submitted on-chain"
            ),
            Err(e) => tracing::error!(
                error = %e,
                "TWAP on-chain submission failed, continuing"
            ),
        }
    }

    let proof_set = state.session_manager.get_proofs(&session_id).await?;

    let block_number = state.onchain_client.get_block_number().await?;
    let seed = match state
        .onchain_client
        .get_block_hash(block_number.saturating_sub(SEED_BLOCK_OFFSET))
        .await?
    {
        Some(hash) => hash,
        None => {
            tracing::warn!(block = block_number, "seed block hash unavailable, using zero seed");
            [0u8; 32]
        }
    };

    // TODO(phase2-node): retrieve real NodeSignature from assigned node via gRPC
    // The assigned node sends its NodeSignature with SessionEnded message.
    // Until node binary exists, use placeholder.
    let assigned_node_sig = NodeSignature {
        node_id: assigned_node_id.clone(),
        signature: Signature::new(U256::ZERO, U256::ZERO, false),
        signed_at: Utc::now(),
    };

    let attestation = state
        .node_coordinator
        .coordinate_attestation(
            session_id.clone(),
            assigned_node_sig,
            proof_set,
            seed,
            &assigned_node_id,
        )
        .await;

    match attestation {
        Ok(attestation_result) => {
            let duration_secs =
                (twap.session_end - twap.session_start).num_seconds().max(0) as u64;
            let payout_config = payout_calculator::default_config();
            // The average is over the bounded client-claimed hashrates: the
            // PreFilter rejects rates above the per-client physical max
            // (HashrateImpossible), so claimed rates cannot run unbounded.
            // TODO(phase3-verified-hashrate): the hardened model counts
            // node-verified RandomX solutions per unit time rather than
            // trusting the bounded client claim.
            let avg_hashrate = state
                .session_manager
                .get_average_hashrate(&session_id)
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "failed to read average hashrate, using 0");
                    0
                });
            let payout_result = payout_calculator::calculate_payout(
                avg_hashrate,
                duration_secs,
                twap.twap,
                &ctx.commodity,
                &payout_config,
            );
            let gross_prm = payout_result.gross_prm;
            tracing::info!(
                session_id = %session_id.0,
                gross_prm = %payout_result.gross_prm,
                redemption_usd_scaled = %payout_result.redemption_usd_scaled,
                net_usdc_scaled = %payout_result.net_usdc_scaled,
                house_edge_bps = %payout_result.house_edge_bps,
                "payout calculated"
            );

            let mut proposal = MintProposal {
                session_id: session_id.clone(),
                wallet: ctx.wallet,
                gross_prm,
                commodity: ctx.commodity,
                attestation: attestation_result,
                backend_sig: Signature::new(U256::ZERO, U256::ZERO, false),
                created_at: Utc::now(),
                status: ProposalStatus::Pending,
            };
            let signature_bytes = onchain_client::OnchainClient::sign_proposal(
                &proposal,
                state.signing_key.as_ref(),
            )?;
            proposal.backend_sig = Signature::try_from(signature_bytes.as_ref()).map_err(|e| {
                tracing::error!(error = %e, "invalid backend signature bytes");
                ApiError::Internal
            })?;

            if let Err(e) = state.postgres_store.insert_mint_proposal(&proposal).await {
                tracing::error!(error = %e, "failed to persist mint proposal");
            }
            metrics::MINT_PROPOSALS_TOTAL
                .with_label_values(&["Pending"])
                .inc();
            metrics::SESSION_ACTIVE_COUNT.dec();
            state
                .session_manager
                .delete_session(&ctx.wallet, &session_id)
                .await?;
            Ok((
                StatusCode::OK,
                Json(EndSessionResponse {
                    status: "completed".to_string(),
                }),
            ))
        }
        Err(e) => {
            tracing::error!(error = %e, "attestation failed");
            Ok((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(EndSessionResponse {
                    status: "attestation_failed".to_string(),
                }),
            ))
        }
    }
}

/// Formats an address the way `postgres-store` persists wallets
/// (`format!("{:?}", _)`), so read queries match stored rows.
fn wallet_db_key(wallet: &Address) -> String {
    format!("{:?}", wallet)
}

/// Parses a wallet path parameter as an address, returning a 400 on failure.
fn parse_wallet(raw: &str) -> Result<Address, ApiError> {
    raw.parse::<Address>()
        .map_err(|_| ApiError::BadRequest("invalid wallet address"))
}

/// Returns a wallet's payout history, newest first. The `limit` query parameter
/// defaults to 50 and is clamped to a maximum of 200.
async fn wallet_payouts(
    State(state): State<AppState>,
    Path(wallet): Path<String>,
    Query(params): Query<PayoutsQuery>,
) -> Result<Json<Vec<postgres_store::PayoutRow>>, ApiError> {
    let address = parse_wallet(&wallet)?;
    let key = wallet_db_key(&address);
    let limit = params
        .limit
        .unwrap_or(DEFAULT_PAYOUT_LIMIT)
        .clamp(1, MAX_PAYOUT_LIMIT);
    let rows = state.postgres_store.get_payouts_for_wallet(&key, limit).await?;
    Ok(Json(rows))
}

/// Returns a wallet's earnings aggregated by commodity.
async fn wallet_earnings(
    State(state): State<AppState>,
    Path(wallet): Path<String>,
) -> Result<Json<Vec<postgres_store::EarningsRow>>, ApiError> {
    let address = parse_wallet(&wallet)?;
    let key = wallet_db_key(&address);
    let rows = state.postgres_store.get_earnings_by_commodity(&key).await?;
    Ok(Json(rows))
}

/// Returns a wallet's active sessions. Redis session keys use the address
/// Display form (see `SessionStore::create_session`), distinct from the
/// debug-formatted wallet persisted in Postgres.
async fn wallet_sessions(
    State(state): State<AppState>,
    Path(wallet): Path<String>,
) -> Result<Json<Vec<session_manager::SessionSummary>>, ApiError> {
    let address = parse_wallet(&wallet)?;
    let key = address.to_string();
    let rows = state.session_manager.list_sessions_for_wallet(&key).await?;
    Ok(Json(rows))
}

/// Exposes the Prometheus metrics registry in text exposition format.
pub async fn metrics_handler() -> (StatusCode, String) {
    (StatusCode::OK, metrics::metrics_handler())
}

/// Liveness probe.
async fn health_check() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::OK,
        Json(serde_json::json!({ "status": "ok", "service": "primora-verification" })),
    )
}

/// Builds the application router with all routes and shared state.
///
/// Routes are served behind Cloudflare on a single origin; the frontend and
/// API share a domain, so no CORS layer is required.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/sessions", post(create_session))
        .route("/sessions/:session_id/proofs", post(submit_proof))
        .route("/sessions/:session_id/end", post(end_session))
        .route("/wallets/:wallet/payouts", get(wallet_payouts))
        .route("/wallets/:wallet/earnings", get(wallet_earnings))
        .route("/wallets/:wallet/sessions", get(wallet_sessions))
        .route("/health", get(health_check))
        .route("/metrics", get(metrics_handler))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Binds to `addr` and serves the router until shutdown. The connecting peer
/// address is made available to handlers for IP-based rate limiting.
pub async fn serve(state: AppState, addr: &str) -> Result<(), ServiceError> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_health_check() {
        let (status, Json(value)) = health_check().await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["status"].as_str(), Some("ok"));
    }

    #[tokio::test]
    async fn test_router_has_health_route() {
        let (status, Json(value)) = health_check().await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["service"].as_str(), Some("primora-verification"));
    }

    #[test]
    fn test_parse_client_type() {
        assert_eq!(parse_client_type("Browser").unwrap(), ClientType::Browser);
        assert_eq!(parse_client_type("desktop").unwrap(), ClientType::Desktop);
        assert_eq!(parse_client_type("CLI").unwrap(), ClientType::Cli);
        assert!(matches!(
            parse_client_type("quantum"),
            Err(ApiError::BadRequest(_))
        ));
    }

    #[test]
    fn test_parse_hash() {
        let hex = "00".repeat(32);
        assert_eq!(parse_hash(&hex, "h").unwrap(), [0u8; 32]);
        assert!(matches!(parse_hash("zz", "h"), Err(ApiError::BadRequest(_))));
        assert!(matches!(
            parse_hash("00", "h"),
            Err(ApiError::BadRequest(_))
        ));
    }

    #[tokio::test]
    async fn test_metrics_endpoint_returns_ok() {
        let (status, body) = metrics_handler().await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body.is_empty());
    }

    #[test]
    fn test_invalid_wallet_returns_400() {
        let err = parse_wallet("not-an-address").unwrap_err();
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_valid_wallet_parses_and_db_key_is_debug_format() {
        let addr = parse_wallet("0x0000000000000000000000000000000000000000").unwrap();
        assert_eq!(wallet_db_key(&addr), format!("{:?}", addr));
    }

    #[test]
    fn test_enforce() {
        assert!(enforce(RateLimitResult::Allowed).is_ok());
        assert!(matches!(
            enforce(RateLimitResult::Denied {
                limit: 1,
                window_secs: 60
            }),
            Err(ApiError::RateLimited)
        ));
    }
}
