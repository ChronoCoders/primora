#![deny(warnings)]
#![deny(missing_docs)]
//! Axum entry point: router, application state, and request routing.

use std::net::SocketAddr;
use std::sync::Arc;

use alloy_primitives::Address;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use common::{
    ClientType, PartialProof, ProofValidator, SessionContext, SessionId, ValidationMode,
    ValidationResult,
};
use proof_validator::PreFilterValidator;
use rate_limiter::RateLimitResult;
use serde::{Deserialize, Serialize};
use tower_http::trace::TraceLayer;

/// Shared application state injected into every handler.
#[derive(Clone)]
pub struct AppState {
    /// Redis-backed session store.
    pub session_manager: Arc<session_manager::SessionStore>,
    /// Per-wallet, per-IP, per-node rate limiter.
    pub rate_limiter: Arc<rate_limiter::RateLimiter>,
    /// Anomaly scoring and event publisher.
    pub anomaly_engine: Arc<anomaly_engine::AnomalyEngine>,
    /// Per-block mint ceiling calculator.
    pub mint_ceiling: Arc<mint_ceiling::MintCeilingCalculator>,
    /// Read-only on-chain client.
    pub onchain_client: Arc<onchain_client::OnchainClient>,
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
    /// Hex-encoded proof hash.
    pub proof_hash: String,
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

/// Parses the client type from its lowercase string label.
fn parse_client_type(raw: &str) -> Result<ClientType, ApiError> {
    match raw.to_ascii_lowercase().as_str() {
        "browser" => Ok(ClientType::Browser),
        "desktop" => Ok(ClientType::Desktop),
        "cli" => Ok(ClientType::Cli),
        _ => Err(ApiError::BadRequest("unknown client_type")),
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

    let ip = peer.ip();
    enforce(state.rate_limiter.check_wallet(&wallet).await?)?;
    enforce(state.rate_limiter.check_ip(&ip.to_string()).await?)?;

    let active_sessions_count = state.session_manager.get_active_session_count(&wallet).await?;
    let ctx = SessionContext {
        wallet,
        ip: Some(ip),
        client_type,
        active_sessions_count,
        last_submission_at: None,
        recent_proof_count: 0,
    };
    let session_id = state.session_manager.create_session(&ctx).await?;
    state
        .session_manager
        .set_commit(&session_id, commit_hash)
        .await?;

    Ok((
        StatusCode::CREATED,
        Json(CreateSessionResponse {
            session_id: session_id.0,
        }),
    ))
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

    let proof = PartialProof {
        session_id: session_id.clone(),
        wallet: ctx.wallet,
        sequence: body.sequence,
        hashrate: body.hashrate,
        proof_hash,
        submitted_at: Utc::now(),
        signature: None,
    };
    let result = PreFilterValidator.validate(&proof, ValidationMode::PreFilter, &ctx);
    state.session_manager.increment_proof_count(&session_id).await?;

    let accepted = !matches!(result, ValidationResult::Invalid(_));
    let level = state
        .anomaly_engine
        .process(session_id, ctx.wallet, vec![result]);

    Ok(Json(SubmitProofResponse {
        accepted,
        suspicion_level: format!("{level:?}"),
    }))
}

/// Ends a session: verifies the commit-reveal nonce and, on success, finalizes
/// the session.
///
/// A nonce that does not match the stored commit returns `status: "rejected"`.
///
/// TODO(attestation): on a verified reveal this must collect the session proof
/// set, run [`node_coordinator::NodeCoordinator::coordinate_attestation`], and
/// produce a signed [`common::MintProposal`]. That path is blocked on the gRPC
/// `NodeClient` implementation, the backend signing key in [`AppState`], and the
/// eligible-node configuration, none of which exist yet.
async fn end_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(body): Json<EndSessionRequest>,
) -> Result<Json<EndSessionResponse>, ApiError> {
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
        return Ok(Json(EndSessionResponse {
            status: "rejected".to_string(),
        }));
    }

    state
        .session_manager
        .delete_session(&ctx.wallet, &session_id)
        .await?;
    Ok(Json(EndSessionResponse {
        status: "verified".to_string(),
    }))
}

/// Liveness probe.
async fn health_check() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::OK,
        Json(serde_json::json!({ "status": "ok", "service": "primora-verification" })),
    )
}

/// Builds the application router with all routes and shared state.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/sessions", post(create_session))
        .route("/sessions/:session_id/proofs", post(submit_proof))
        .route("/sessions/:session_id/end", post(end_session))
        .route("/health", get(health_check))
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
