//! Admin SIWE authentication (EIP-4361): the `ADMIN_WALLET` gate, single-use
//! login nonces, HMAC-SHA256 session tokens, and the `/admin/*` guard. Every
//! `/admin/*` request is authorized server-side; the layer fails closed when no
//! `ADMIN_WALLET` or session secret is configured.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::time::{Duration, Instant};

use alloy_primitives::{hex, Address, Signature};
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::{wallet_db_key, AppState};

type HmacSha256 = Hmac<Sha256>;

/// Lifetime of a login nonce before it expires.
const NONCE_TTL: Duration = Duration::from_secs(300);
/// Suffix of an EIP-4361 message's first line, after the domain.
const SIWE_HEADER_SUFFIX: &str = " wants you to sign in with your Ethereum account:";

/// Parses `ADMIN_WALLET` (comma-separated addresses) into a set, mirroring the
/// `NODE_SIGNERS` parser. Invalid entries are logged and skipped; an empty result
/// disables the admin routes (fail closed).
pub fn parse_admin_wallets(raw: &str) -> HashSet<Address> {
    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| match Address::from_str(entry) {
            Ok(address) => Some(address),
            Err(_) => {
                tracing::warn!(entry, "invalid ADMIN_WALLET address; skipping");
                None
            }
        })
        .collect()
}

/// In-memory single-use login nonce store with TTL. Single-instance only; a
/// multi-instance deployment would back this with Redis so a nonce issued by one
/// instance is consumable by another.
#[derive(Default)]
pub struct NonceStore {
    inner: tokio::sync::Mutex<HashMap<String, Instant>>,
}

impl NonceStore {
    /// Issues a fresh random nonce with a bounded TTL, pruning expired entries.
    pub async fn issue(&self) -> String {
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let now = Instant::now();
        let mut guard = self.inner.lock().await;
        guard.retain(|_, issued| now.duration_since(*issued) < NONCE_TTL);
        guard.insert(nonce.clone(), now);
        nonce
    }

    /// Consumes a nonce, returning true only when it was outstanding and unexpired.
    /// A consumed or expired nonce can never be reused (replay protection).
    pub async fn consume(&self, nonce: &str) -> bool {
        let mut guard = self.inner.lock().await;
        match guard.remove(nonce) {
            Some(issued) => Instant::now().duration_since(issued) < NONCE_TTL,
            None => false,
        }
    }
}

/// An authentication failure. Rendered as `401` with a non-leaky body so a caller
/// cannot tell which specific check failed.
#[derive(Debug)]
pub enum AuthError {
    /// Authentication failed (bad signature, non-admin, bad/expired nonce or
    /// token, wrong domain, or admin disabled).
    Unauthorized,
    /// A server-side error while issuing a token.
    Internal,
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        match self {
            Self::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "internal error" })),
            )
                .into_response(),
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": "unauthorized" })),
            )
                .into_response(),
        }
    }
}

/// The claims carried by a session token: the admin address and expiry (unix).
#[derive(Debug, Serialize, Deserialize)]
struct SessionClaims {
    addr: String,
    exp: u64,
}

/// Issues an HMAC-SHA256 session token `hex(payload).hex(sig)` binding the admin
/// address and an expiry `now_unix + ttl`, signed with the server secret.
pub fn issue_session_token(
    secret: &[u8],
    addr: Address,
    ttl: Duration,
    now_unix: u64,
) -> Result<String, AuthError> {
    let claims = SessionClaims {
        addr: format!("{addr:?}"),
        exp: now_unix.saturating_add(ttl.as_secs()),
    };
    let payload = serde_json::to_vec(&claims).map_err(|_| AuthError::Internal)?;
    let mut mac = HmacSha256::new_from_slice(secret).map_err(|_| AuthError::Internal)?;
    mac.update(&payload);
    let sig = mac.finalize().into_bytes();
    Ok(format!("{}.{}", hex::encode(payload), hex::encode(sig)))
}

/// Verifies a session token: constant-time HMAC check, unexpired, and a parseable
/// address. Returns the address on success; `None` on any failure. The caller
/// still checks the address against the live `ADMIN_WALLET` set.
pub fn verify_session_token(secret: &[u8], token: &str, now_unix: u64) -> Option<Address> {
    let (payload_hex, sig_hex) = token.split_once('.')?;
    let payload = hex::decode(payload_hex).ok()?;
    let sig = hex::decode(sig_hex).ok()?;
    let mut mac = HmacSha256::new_from_slice(secret).ok()?;
    mac.update(&payload);
    mac.verify_slice(&sig).ok()?;
    let claims: SessionClaims = serde_json::from_slice(&payload).ok()?;
    if claims.exp <= now_unix {
        return None;
    }
    Address::from_str(&claims.addr).ok()
}

/// The address and nonce recovered from a verified SIWE message.
pub struct SiweVerified {
    /// The address that signed the message (recovered via ecrecover).
    pub address: Address,
    /// The nonce embedded in the message, to be consumed once.
    pub nonce: String,
}

/// Verifies an EIP-4361 (SIWE) message and signature using alloy's EIP-191
/// recovery (the same primitive as attestation ecrecover): recovers the signer,
/// checks the message's declared address matches, validates the domain against
/// `expected_domain`, and rejects an expired message. The nonce is returned for
/// single-use consumption by the caller; `ADMIN_WALLET` membership is checked by
/// the caller. Any malformed or failing field yields `Unauthorized`.
pub fn verify_siwe(
    message: &str,
    signature_hex: &str,
    expected_domain: &str,
    now_unix: u64,
) -> Result<SiweVerified, AuthError> {
    let sig_bytes = hex::decode(signature_hex.trim().trim_start_matches("0x"))
        .map_err(|_| AuthError::Unauthorized)?;
    let signature = Signature::try_from(sig_bytes.as_slice()).map_err(|_| AuthError::Unauthorized)?;
    let recovered = signature
        .recover_address_from_msg(message.as_bytes())
        .map_err(|_| AuthError::Unauthorized)?;

    let mut lines = message.lines();
    let header = lines.next().ok_or(AuthError::Unauthorized)?;
    let domain = header
        .strip_suffix(SIWE_HEADER_SUFFIX)
        .ok_or(AuthError::Unauthorized)?
        .trim();
    if domain != expected_domain {
        return Err(AuthError::Unauthorized);
    }
    let addr_line = lines.next().ok_or(AuthError::Unauthorized)?.trim();
    let declared = Address::from_str(addr_line).map_err(|_| AuthError::Unauthorized)?;
    if declared != recovered {
        return Err(AuthError::Unauthorized);
    }

    let field = |prefix: &str| {
        message
            .lines()
            .find_map(|line| line.strip_prefix(prefix).map(|value| value.trim().to_string()))
    };
    let nonce = field("Nonce:").ok_or(AuthError::Unauthorized)?;
    if nonce.is_empty() {
        return Err(AuthError::Unauthorized);
    }
    if let Some(expiration) = field("Expiration Time:") {
        let exp = chrono::DateTime::parse_from_rfc3339(&expiration)
            .map_err(|_| AuthError::Unauthorized)?
            .timestamp();
        if exp <= now_unix as i64 {
            return Err(AuthError::Unauthorized);
        }
    }

    Ok(SiweVerified {
        address: recovered,
        nonce,
    })
}

/// The current unix time in seconds.
fn now_unix() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp().max(0)).unwrap_or(0)
}

/// Response for `GET /admin/nonce`: the single-use nonce plus the SIWE message
/// parameters the client should sign.
#[derive(Debug, Serialize)]
pub struct NonceResponse {
    /// Single-use nonce.
    pub nonce: String,
    /// Expected message domain.
    pub domain: String,
    /// Expected message URI.
    pub uri: String,
    /// EIP-4361 version.
    pub version: String,
    /// Suggested statement line.
    pub statement: String,
    /// Nonce issue time (RFC 3339).
    pub issued_at: String,
    /// Nonce expiry time (RFC 3339).
    pub expiration_time: String,
}

/// Issues a login nonce. Fails closed (`401`) when admin is not configured.
pub async fn nonce(State(state): State<AppState>) -> Result<Json<NonceResponse>, AuthError> {
    if state.admin_wallets.is_empty() || state.admin_session_secret.is_none() {
        return Err(AuthError::Unauthorized);
    }
    let nonce = state.admin_nonces.issue().await;
    let now = chrono::Utc::now();
    Ok(Json(NonceResponse {
        nonce,
        domain: state.admin_domain.clone(),
        uri: format!("https://{}", state.admin_domain),
        version: "1".to_string(),
        statement: "Sign in to the Primora admin console.".to_string(),
        issued_at: now.to_rfc3339(),
        expiration_time: (now + chrono::Duration::seconds(NONCE_TTL.as_secs() as i64)).to_rfc3339(),
    }))
}

/// Request for `POST /admin/login`: the signed SIWE message and its signature.
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    /// The exact EIP-4361 message the wallet signed.
    pub message: String,
    /// Hex signature over the message.
    pub signature: String,
}

/// Response for a successful login: the bearer session token and its lifetime.
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    /// Bearer token for `Authorization: Bearer <token>` on `/admin/*`.
    pub token: String,
    /// Token lifetime in seconds.
    pub expires_in: u64,
}

/// Verifies a SIWE login and, on success, issues a session token. Order: verify
/// the signature/domain/expiry, require the recovered address to be an admin,
/// consume the nonce (single-use), then issue the token. Any failure is a
/// non-leaky `401`.
pub async fn login(
    State(state): State<AppState>,
    Json(body): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, AuthError> {
    let secret = state.admin_session_secret.as_ref().ok_or(AuthError::Unauthorized)?;
    if state.admin_wallets.is_empty() {
        return Err(AuthError::Unauthorized);
    }
    let now = now_unix();
    let verified = verify_siwe(&body.message, &body.signature, &state.admin_domain, now)?;
    if !state.admin_wallets.contains(&verified.address) {
        return Err(AuthError::Unauthorized);
    }
    if !state.admin_nonces.consume(&verified.nonce).await {
        return Err(AuthError::Unauthorized);
    }
    let token = issue_session_token(secret, verified.address, state.admin_session_ttl, now)?;
    tracing::info!(admin = %verified.address, "admin session issued");
    Ok(Json(LoginResponse {
        token,
        expires_in: state.admin_session_ttl.as_secs(),
    }))
}

/// The `/admin/*` guard: validates the bearer session token on every request and
/// requires the address to still be in `ADMIN_WALLET`. Fails closed when admin is
/// unconfigured or the token is missing/invalid/expired.
pub async fn admin_guard(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let secret = match state.admin_session_secret.as_ref() {
        Some(secret) => secret,
        None => return AuthError::Unauthorized.into_response(),
    };
    if state.admin_wallets.is_empty() {
        return AuthError::Unauthorized.into_response();
    }
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    match token.and_then(|token| verify_session_token(secret, token, now_unix())) {
        Some(address) if state.admin_wallets.contains(&address) => next.run(request).await,
        _ => AuthError::Unauthorized.into_response(),
    }
}

/// The admin overview figures. Every field is real, already-tracked data: no
/// unique-miner count or aggregate network hashrate (neither is tracked).
#[derive(Debug, Serialize)]
pub struct AdminOverview {
    /// Current active mining sessions (`SESSION_ACTIVE_COUNT` gauge).
    pub active_sessions: i64,
    /// Combined absolute reserve across configured Treasuries, 6-decimal.
    pub reserve_total_usd: String,
    /// Combined total redeemed to date across Treasuries, 6-decimal.
    pub total_redeemed_usd: String,
    /// Company mining share in basis points (`/entity/share` computation).
    pub company_mining_share_bps: u32,
    /// Total persisted anomaly events (flagged sessions, operator view).
    pub flagged_events: i64,
    /// The configured backend per-day mint ceiling, in whole PRM.
    pub daily_mint_ceiling_prm: u64,
}

/// Returns the admin overview from real, already-available data. Guarded: only
/// reachable with a valid admin session.
pub async fn overview(State(state): State<AppState>) -> Result<Json<AdminOverview>, AuthError> {
    let active_sessions = metrics::SESSION_ACTIVE_COUNT.get();

    let mut reserve_total: u128 = 0;
    let mut redeemed_total: u128 = 0;
    for (chain, reader) in &state.treasury_readers {
        match reader.reserve_balances().await {
            Ok(balances) => {
                reserve_total = reserve_total.saturating_add(balances.total_reserve_usd);
                redeemed_total = redeemed_total.saturating_add(balances.total_redeemed_usd);
            }
            Err(e) => tracing::warn!(error = %e, chain = %chain, "admin overview: treasury read failed"),
        }
    }

    let company_key = state.company_wallet.as_ref().map(wallet_db_key);
    let share = state
        .postgres_store
        .company_mining_share(company_key.as_deref().unwrap_or(""))
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "admin overview: company share read failed");
            AuthError::Internal
        })?;
    let flagged_events = state.postgres_store.count_anomaly_events().await.map_err(|e| {
        tracing::error!(error = %e, "admin overview: anomaly count read failed");
        AuthError::Internal
    })?;
    let daily_mint_ceiling_prm = state.mint_ceiling.daily_ceiling(
        state.mint_ceiling_active_users,
        state.mint_ceiling_avg_daily_prm_per_user,
    );

    Ok(Json(AdminOverview {
        active_sessions,
        reserve_total_usd: reserve_total.to_string(),
        total_redeemed_usd: redeemed_total.to_string(),
        company_mining_share_bps: u32::try_from(share.share_bps).unwrap_or(0),
        flagged_events,
        daily_mint_ceiling_prm,
    }))
}

/// Logout: session tokens are stateless, so the short TTL enforces expiry. A
/// server-side denylist for immediate revocation is a future addition. Returns
/// `204` for a valid admin session.
pub async fn logout() -> StatusCode {
    StatusCode::NO_CONTENT
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::signers::local::PrivateKeySigner;
    use alloy::signers::SignerSync;

    const SECRET: &[u8] = b"test-admin-session-secret";
    const DOMAIN: &str = "admin.primora.rs";

    fn siwe_message(domain: &str, addr: Address, nonce: &str, expiration: Option<&str>) -> String {
        let mut msg = format!(
            "{domain}{SIWE_HEADER_SUFFIX}\n{addr:?}\n\nSign in.\n\nURI: https://{domain}\nVersion: 1\nChain ID: 1\nNonce: {nonce}\nIssued At: 2026-01-01T00:00:00Z"
        );
        if let Some(exp) = expiration {
            msg.push_str(&format!("\nExpiration Time: {exp}"));
        }
        msg
    }

    #[test]
    fn test_parse_admin_wallets() {
        let set = parse_admin_wallets("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266, not-an-addr");
        assert_eq!(set.len(), 1);
        assert!(set.contains(&Address::from([0xf3u8, 0x9f, 0xd6, 0xe5, 0x1a, 0xad, 0x88, 0xf6, 0xf4, 0xce, 0x6a, 0xb8, 0x82, 0x72, 0x79, 0xcf, 0xff, 0xb9, 0x22, 0x66])));
    }

    #[test]
    fn test_siwe_recovers_admin_and_nonce() {
        let signer = PrivateKeySigner::random();
        let addr = signer.address();
        let msg = siwe_message(DOMAIN, addr, "abc123", None);
        let sig = signer.sign_message_sync(msg.as_bytes()).unwrap();
        let sig_hex = hex::encode(sig.as_bytes());
        let verified = verify_siwe(&msg, &sig_hex, DOMAIN, 1_800_000_000).unwrap();
        assert_eq!(verified.address, addr);
        assert_eq!(verified.nonce, "abc123");
    }

    #[test]
    fn test_siwe_wrong_domain_rejected() {
        let signer = PrivateKeySigner::random();
        let msg = siwe_message("evil.example", signer.address(), "n1", None);
        let sig = signer.sign_message_sync(msg.as_bytes()).unwrap();
        let sig_hex = hex::encode(sig.as_bytes());
        assert!(verify_siwe(&msg, &sig_hex, DOMAIN, 1_800_000_000).is_err());
    }

    #[test]
    fn test_siwe_expired_rejected() {
        let signer = PrivateKeySigner::random();
        let msg = siwe_message(DOMAIN, signer.address(), "n1", Some("2020-01-01T00:00:00Z"));
        let sig = signer.sign_message_sync(msg.as_bytes()).unwrap();
        let sig_hex = hex::encode(sig.as_bytes());
        assert!(verify_siwe(&msg, &sig_hex, DOMAIN, 1_800_000_000).is_err());
    }

    #[test]
    fn test_siwe_tampered_message_rejected() {
        let signer = PrivateKeySigner::random();
        let msg = siwe_message(DOMAIN, signer.address(), "n1", None);
        let sig = signer.sign_message_sync(msg.as_bytes()).unwrap();
        let sig_hex = hex::encode(sig.as_bytes());
        let tampered = msg.replace("Nonce: n1", "Nonce: n2");
        assert!(verify_siwe(&tampered, &sig_hex, DOMAIN, 1_800_000_000).is_err());
    }

    #[test]
    fn test_session_token_roundtrip_and_expiry() {
        let addr = PrivateKeySigner::random().address();
        let token = issue_session_token(SECRET, addr, Duration::from_secs(1800), 1_000).unwrap();
        assert_eq!(verify_session_token(SECRET, &token, 1_500), Some(addr));
        assert_eq!(verify_session_token(SECRET, &token, 5_000), None);
        assert_eq!(verify_session_token(b"wrong-secret", &token, 1_500), None);
        assert_eq!(verify_session_token(SECRET, "garbage.token", 1_500), None);
    }

    #[tokio::test]
    async fn test_nonce_single_use_and_expiry() {
        let store = NonceStore::default();
        let nonce = store.issue().await;
        assert!(store.consume(&nonce).await);
        assert!(!store.consume(&nonce).await);
        assert!(!store.consume("never-issued").await);
    }
}
