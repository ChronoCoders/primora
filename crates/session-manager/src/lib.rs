#![deny(warnings)]
#![deny(missing_docs)]
//! Redis-backed commit-reveal and session lifecycle management.

use alloy_primitives::Address;
use chrono::{DateTime, Utc};
use common::{Chain, ClientType, PartialProof, SessionContext, SessionId};
use redis::aio::MultiplexedConnection;
use serde::Serialize;
use sha2::{Digest, Sha256};

const TTL_SECS: i64 = 3600;
const COMMIT_PENDING: &str = "pending";

/// Summary of an active session for a wallet, for the Overview page.
#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    /// Session identifier.
    pub session_id: String,
    /// Backing commodity name.
    pub commodity: String,
    /// Number of proofs counted for the session.
    pub proof_count: u32,
    /// Average hashrate over the session so far (H/s), derived from proof
    /// submissions. This is a running average, not an instantaneous rate; it is
    /// 0 until the first proof is counted.
    pub avg_hashrate: u64,
    /// The client software type for this session (lowercase, e.g. `desktop`),
    /// from the stored session context.
    pub client_type: String,
    /// The chain this session mints to.
    pub target_chain: Chain,
    /// UTC timestamp when the session was created.
    pub started_at: DateTime<Utc>,
    /// Timestamp of the last accepted proof submission, sourced from the
    /// `last_activity:{session_id}` key. `None` until the first proof.
    pub last_submission_at: Option<DateTime<Utc>>,
    /// Session lifecycle status; a session present in Redis is active.
    pub status: String,
    /// CPU worker threads for this session (0 if not reported).
    pub cpu_threads: u32,
    /// Submitted proofs that passed pre-filter validation. Pre-filter
    /// verification only; full 2-of-3 node attestation is an `end_session` event.
    #[serde(default)]
    pub verified_proof_count: u32,
    /// Submitted proofs rejected by pre-filter validation (judged invalid).
    #[serde(default)]
    pub rejected_proof_count: u32,
    /// Net USD (cents) this session has earned so far: grounded in elapsed time,
    /// average hashrate, live TWAP, staking boost, and the 17% house edge. Not a
    /// full-day projection. Populated by the service layer; 0 until then or when
    /// no TWAP price is available yet.
    #[serde(default)]
    pub est_net_usd_cents: i64,
    /// Site code of the assigned node (e.g. `JHB`), if known.
    #[serde(default)]
    pub site_code: Option<String>,
    /// City of the assigned node (e.g. `Johannesburg`), if known.
    #[serde(default)]
    pub site_city: Option<String>,
    /// Whether the session is currently paused (proofs rejected until resumed).
    #[serde(default)]
    pub paused: bool,
}

/// Errors returned by the session store.
#[derive(Debug)]
pub enum SessionManagerError {
    /// Redis transport or command error.
    Redis(redis::RedisError),
    /// JSON serialization or deserialization error.
    Serialization(serde_json::Error),
}

impl std::fmt::Display for SessionManagerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Redis(e) => write!(f, "redis error: {e}"),
            Self::Serialization(e) => write!(f, "serialization error: {e}"),
        }
    }
}

impl std::error::Error for SessionManagerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Redis(e) => Some(e),
            Self::Serialization(e) => Some(e),
        }
    }
}

impl From<redis::RedisError> for SessionManagerError {
    fn from(e: redis::RedisError) -> Self {
        Self::Redis(e)
    }
}

impl From<serde_json::Error> for SessionManagerError {
    fn from(e: serde_json::Error) -> Self {
        Self::Serialization(e)
    }
}

/// Redis-backed store for session state, commit-reveal, and proof counters.
pub struct SessionStore {
    conn: MultiplexedConnection,
}

impl SessionStore {
    /// Opens a multiplexed async connection to the Redis instance at `url`.
    pub async fn new(url: &str) -> Result<Self, redis::RedisError> {
        let client = redis::Client::open(url)?;
        let conn = client.get_multiplexed_async_connection().await?;
        Ok(Self { conn })
    }

    /// Creates a new session, persisting context, reverse lookup, and a pending
    /// commit placeholder, each with a one-hour TTL.
    pub async fn create_session(
        &self,
        ctx: &SessionContext,
    ) -> Result<SessionId, SessionManagerError> {
        let session_id = SessionId(uuid::Uuid::new_v4().to_string());
        let wallet = format!("{}", ctx.wallet);
        let payload = serde_json::to_string(ctx)?;
        let mut conn = self.conn.clone();
        let session_key = format!("session:{wallet}:{}", session_id.0);
        let lookup_key = format!("session_lookup:{}", session_id.0);
        let commit_key = format!("commit:{}", session_id.0);
        let _: () = redis::cmd("SET")
            .arg(&session_key)
            .arg(&payload)
            .arg("EX")
            .arg(TTL_SECS)
            .query_async(&mut conn)
            .await?;
        let _: () = redis::cmd("SET")
            .arg(&lookup_key)
            .arg(&wallet)
            .arg("EX")
            .arg(TTL_SECS)
            .query_async(&mut conn)
            .await?;
        let _: () = redis::cmd("SET")
            .arg(&commit_key)
            .arg(COMMIT_PENDING)
            .arg("EX")
            .arg(TTL_SECS)
            .query_async(&mut conn)
            .await?;
        tracing::debug!(session = %session_id.0, "created session");
        Ok(session_id)
    }

    /// Loads a session by id via the reverse lookup, returning `None` if either
    /// the lookup or the session record is absent.
    pub async fn get_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionContext>, SessionManagerError> {
        let mut conn = self.conn.clone();
        let lookup_key = format!("session_lookup:{}", session_id.0);
        let wallet: Option<String> = redis::cmd("GET")
            .arg(&lookup_key)
            .query_async(&mut conn)
            .await?;
        let wallet = match wallet {
            Some(w) => w,
            None => return Ok(None),
        };
        let session_key = format!("session:{wallet}:{}", session_id.0);
        let payload: Option<String> = redis::cmd("GET")
            .arg(&session_key)
            .query_async(&mut conn)
            .await?;
        match payload {
            Some(p) => Ok(Some(serde_json::from_str(&p)?)),
            None => Ok(None),
        }
    }

    /// Stores the hex-encoded commit hash for a session with a one-hour TTL.
    pub async fn set_commit(
        &self,
        session_id: &SessionId,
        commit_hash: [u8; 32],
    ) -> Result<(), SessionManagerError> {
        let mut conn = self.conn.clone();
        let commit_key = format!("commit:{}", session_id.0);
        let value = alloy_primitives::hex::encode(commit_hash);
        let _: () = redis::cmd("SET")
            .arg(&commit_key)
            .arg(&value)
            .arg("EX")
            .arg(TTL_SECS)
            .query_async(&mut conn)
            .await?;
        Ok(())
    }

    /// Verifies a revealed nonce against the stored commit. Returns `false` when
    /// the commit is missing or still pending.
    pub async fn verify_reveal(
        &self,
        session_id: &SessionId,
        nonce: &[u8],
    ) -> Result<bool, SessionManagerError> {
        let mut conn = self.conn.clone();
        let commit_key = format!("commit:{}", session_id.0);
        let stored: Option<String> = redis::cmd("GET")
            .arg(&commit_key)
            .query_async(&mut conn)
            .await?;
        let stored = match stored {
            Some(s) if s != COMMIT_PENDING => s,
            _ => return Ok(false),
        };
        let mut hasher = Sha256::new();
        hasher.update(nonce);
        let computed = alloy_primitives::hex::encode(hasher.finalize());
        Ok(computed == stored)
    }

    /// Appends a partial proof to the session's stored proof list, refreshing the
    /// one-hour TTL. Proofs are persisted as a JSON array under `proofs:{id}`.
    pub async fn store_proof(
        &self,
        session_id: &SessionId,
        proof: PartialProof,
    ) -> Result<(), SessionManagerError> {
        let mut conn = self.conn.clone();
        let key = format!("proofs:{}", session_id.0);
        let existing: Option<String> = redis::cmd("GET").arg(&key).query_async(&mut conn).await?;
        let mut proofs: Vec<PartialProof> = match existing {
            Some(payload) => serde_json::from_str(&payload)?,
            None => Vec::new(),
        };
        proofs.push(proof);
        let payload = serde_json::to_string(&proofs)?;
        let _: () = redis::cmd("SET")
            .arg(&key)
            .arg(&payload)
            .arg("EX")
            .arg(TTL_SECS)
            .query_async(&mut conn)
            .await?;
        Ok(())
    }

    /// Returns all partial proofs stored for a session, or an empty vector when
    /// none have been recorded.
    pub async fn get_proofs(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<PartialProof>, SessionManagerError> {
        let mut conn = self.conn.clone();
        let key = format!("proofs:{}", session_id.0);
        let payload: Option<String> = redis::cmd("GET").arg(&key).query_async(&mut conn).await?;
        match payload {
            Some(p) => Ok(serde_json::from_str(&p)?),
            None => Ok(Vec::new()),
        }
    }

    /// Records an accepted proof toward the server-derived hashrate. The first
    /// accepted proof anchors `hashrate_first_at:{id}` (write-once) and adds no
    /// work; every subsequent accepted proof adds its `difficulty` (the expected
    /// hash count for a linear target = U256::MAX / difficulty) to
    /// `expected_hashes_sum:{id}`. `t_last` is the existing `last_activity:{id}`.
    pub async fn record_accepted_proof(
        &self,
        session_id: &SessionId,
        difficulty: u64,
        now_unix_secs: i64,
    ) -> Result<(), SessionManagerError> {
        let mut conn = self.conn.clone();
        let first_key = format!("hashrate_first_at:{}", session_id.0);
        let anchored: Option<String> = redis::cmd("SET")
            .arg(&first_key)
            .arg(now_unix_secs)
            .arg("NX")
            .arg("EX")
            .arg(TTL_SECS)
            .query_async(&mut conn)
            .await?;
        if anchored.is_some() {
            return Ok(());
        }
        let sum_key = format!("expected_hashes_sum:{}", session_id.0);
        let _: i64 = redis::cmd("INCRBY")
            .arg(&sum_key)
            .arg(difficulty)
            .query_async(&mut conn)
            .await?;
        let _: () = redis::cmd("EXPIRE")
            .arg(&sum_key)
            .arg(TTL_SECS)
            .query_async(&mut conn)
            .await?;
        Ok(())
    }

    /// Returns the server-derived session hashrate in H/s: total expected hashes
    /// (`expected_hashes_sum`) divided by elapsed server time
    /// (`last_activity − hashrate_first_at`), clamped to the per-client physical
    /// maximum. Returns 0 until at least two accepted proofs establish a real
    /// elapsed window. Derived from `difficulty` and server timestamps, never
    /// from any client-reported hashrate.
    pub async fn get_session_hashrate(
        &self,
        session_id: &SessionId,
        client_type: ClientType,
    ) -> Result<u64, SessionManagerError> {
        let mut conn = self.conn.clone();
        let first: Option<i64> = redis::cmd("GET")
            .arg(format!("hashrate_first_at:{}", session_id.0))
            .query_async(&mut conn)
            .await?;
        let last: Option<i64> = redis::cmd("GET")
            .arg(format!("last_activity:{}", session_id.0))
            .query_async(&mut conn)
            .await?;
        let sum: Option<i64> = redis::cmd("GET")
            .arg(format!("expected_hashes_sum:{}", session_id.0))
            .query_async(&mut conn)
            .await?;
        match (first, last, sum) {
            (Some(first), Some(last), Some(sum)) if last > first && sum > 0 => {
                let elapsed = (last - first).max(1) as u64;
                let rate = sum.max(0) as u64 / elapsed;
                Ok(rate.min(proof_validator::max_hashrate(client_type)))
            }
            _ => Ok(0),
        }
    }

    /// Records the current UTC time as the session's last activity, stored as
    /// unix seconds under `last_activity:{session_id}` with a one-hour TTL to
    /// match the session lifetime keys.
    pub async fn touch_last_activity(
        &self,
        session_id: &SessionId,
    ) -> Result<(), SessionManagerError> {
        let mut conn = self.conn.clone();
        let key = format!("last_activity:{}", session_id.0);
        let now = Utc::now().timestamp();
        let _: () = redis::cmd("SET")
            .arg(&key)
            .arg(now)
            .arg("EX")
            .arg(TTL_SECS)
            .query_async(&mut conn)
            .await?;
        Ok(())
    }

    /// Returns the session's last activity time, or `None` when no activity has
    /// been recorded. A stored value that fails to parse is logged and treated
    /// as absent.
    pub async fn get_last_activity(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<DateTime<Utc>>, SessionManagerError> {
        let mut conn = self.conn.clone();
        let key = format!("last_activity:{}", session_id.0);
        let stored: Option<i64> = redis::cmd("GET").arg(&key).query_async(&mut conn).await?;
        let Some(secs) = stored else {
            return Ok(None);
        };
        match DateTime::<Utc>::from_timestamp(secs, 0) {
            Some(ts) => Ok(Some(ts)),
            None => {
                tracing::warn!(session = %session_id.0, secs, "invalid last_activity timestamp");
                Ok(None)
            }
        }
    }

    /// Increments and returns the proof counter for a session.
    pub async fn increment_proof_count(
        &self,
        session_id: &SessionId,
    ) -> Result<u32, SessionManagerError> {
        let mut conn = self.conn.clone();
        let key = format!("proof_count:{}", session_id.0);
        let count: i64 = redis::cmd("INCR").arg(&key).query_async(&mut conn).await?;
        Ok(count as u32)
    }

    /// Returns the current proof counter for a session, or 0 when the counter
    /// key is absent.
    pub async fn get_proof_count(
        &self,
        session_id: &SessionId,
    ) -> Result<u32, SessionManagerError> {
        let mut conn = self.conn.clone();
        let key = format!("proof_count:{}", session_id.0);
        let count: Option<i64> = redis::cmd("GET").arg(&key).query_async(&mut conn).await?;
        Ok(count.unwrap_or(0) as u32)
    }

    /// Increments and returns the verified-proof counter for a session: proofs
    /// that passed pre-filter validation. This is pre-filter verification only;
    /// full 2-of-3 node attestation is a separate `end_session` event.
    pub async fn increment_verified_proof_count(
        &self,
        session_id: &SessionId,
    ) -> Result<u32, SessionManagerError> {
        self.incr_session_counter(&format!("verified_proof_count:{}", session_id.0))
            .await
    }

    /// Returns the verified-proof counter for a session, or 0 when absent.
    pub async fn get_verified_proof_count(
        &self,
        session_id: &SessionId,
    ) -> Result<u32, SessionManagerError> {
        self.get_session_counter(&format!("verified_proof_count:{}", session_id.0))
            .await
    }

    /// Increments and returns the rejected-proof counter for a session: proofs
    /// judged invalid by pre-filter validation.
    pub async fn increment_rejected_proof_count(
        &self,
        session_id: &SessionId,
    ) -> Result<u32, SessionManagerError> {
        self.incr_session_counter(&format!("rejected_proof_count:{}", session_id.0))
            .await
    }

    /// Returns the rejected-proof counter for a session, or 0 when absent.
    pub async fn get_rejected_proof_count(
        &self,
        session_id: &SessionId,
    ) -> Result<u32, SessionManagerError> {
        self.get_session_counter(&format!("rejected_proof_count:{}", session_id.0))
            .await
    }

    /// Marks a session paused: subsequent proof submissions are rejected until
    /// resumed. Sets the `paused:{id}` flag with the standard one-hour TTL.
    /// Idempotent.
    pub async fn set_paused(&self, session_id: &SessionId) -> Result<(), SessionManagerError> {
        let mut conn = self.conn.clone();
        let key = format!("paused:{}", session_id.0);
        let _: () = redis::cmd("SET")
            .arg(&key)
            .arg(1)
            .arg("EX")
            .arg(TTL_SECS)
            .query_async(&mut conn)
            .await?;
        Ok(())
    }

    /// Clears a session's paused flag, allowing proofs again. Idempotent.
    pub async fn clear_paused(&self, session_id: &SessionId) -> Result<(), SessionManagerError> {
        let mut conn = self.conn.clone();
        let key = format!("paused:{}", session_id.0);
        let _: () = redis::cmd("DEL").arg(&key).query_async(&mut conn).await?;
        Ok(())
    }

    /// Returns whether a session is currently paused.
    pub async fn is_paused(&self, session_id: &SessionId) -> Result<bool, SessionManagerError> {
        let mut conn = self.conn.clone();
        let key = format!("paused:{}", session_id.0);
        let exists: bool = redis::cmd("EXISTS").arg(&key).query_async(&mut conn).await?;
        Ok(exists)
    }

    async fn incr_session_counter(&self, key: &str) -> Result<u32, SessionManagerError> {
        let mut conn = self.conn.clone();
        let count: i64 = redis::cmd("INCR").arg(key).query_async(&mut conn).await?;
        let _: () = redis::cmd("EXPIRE")
            .arg(key)
            .arg(TTL_SECS)
            .query_async(&mut conn)
            .await?;
        Ok(count.max(0) as u32)
    }

    async fn get_session_counter(&self, key: &str) -> Result<u32, SessionManagerError> {
        let mut conn = self.conn.clone();
        let count: Option<i64> = redis::cmd("GET").arg(key).query_async(&mut conn).await?;
        Ok(count.unwrap_or(0).max(0) as u32)
    }

    /// Counts active sessions for a wallet by scanning matching keys.
    pub async fn get_active_session_count(
        &self,
        wallet: &Address,
    ) -> Result<u32, SessionManagerError> {
        let mut conn = self.conn.clone();
        let pattern = format!("session:{wallet}:*");
        let mut cursor: u64 = 0;
        let mut count: u32 = 0;
        loop {
            let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(&pattern)
                .arg("COUNT")
                .arg(100i64)
                .query_async(&mut conn)
                .await?;
            count += keys.len() as u32;
            cursor = next;
            if cursor == 0 {
                break;
            }
        }
        Ok(count)
    }

    /// Lists active sessions for a wallet, scanning `session:{wallet}:*` keys
    /// (non-blocking SCAN) and projecting each into a [`SessionSummary`].
    ///
    /// `wallet` must be the Display form of the address, matching the key
    /// written by [`SessionStore::create_session`] (distinct from the
    /// debug-formatted wallet persisted in Postgres).
    pub async fn list_sessions_for_wallet(
        &self,
        wallet: &str,
    ) -> Result<Vec<SessionSummary>, SessionManagerError> {
        let mut conn = self.conn.clone();
        let pattern = format!("session:{wallet}:*");
        let prefix = format!("session:{wallet}:");
        let mut cursor: u64 = 0;
        let mut summaries = Vec::new();
        loop {
            let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(&pattern)
                .arg("COUNT")
                .arg(100i64)
                .query_async(&mut conn)
                .await?;
            for key in keys {
                let payload: Option<String> =
                    redis::cmd("GET").arg(&key).query_async(&mut conn).await?;
                let Some(payload) = payload else {
                    continue;
                };
                let ctx: SessionContext = serde_json::from_str(&payload)?;
                let session_id = key.strip_prefix(&prefix).unwrap_or(&key).to_string();
                let count_key = format!("proof_count:{session_id}");
                let count: Option<i64> =
                    redis::cmd("GET").arg(&count_key).query_async(&mut conn).await?;
                let session = SessionId(session_id.clone());
                let last_submission_at = self.get_last_activity(&session).await?;
                let avg_hashrate = self
                    .get_session_hashrate(&session, ctx.client_type)
                    .await?;
                let verified_proof_count = self.get_verified_proof_count(&session).await?;
                let rejected_proof_count = self.get_rejected_proof_count(&session).await?;
                let paused = self.is_paused(&session).await?;
                summaries.push(SessionSummary {
                    session_id,
                    commodity: format!("{:?}", ctx.commodity),
                    proof_count: count.unwrap_or(0).max(0) as u32,
                    avg_hashrate,
                    client_type: format!("{:?}", ctx.client_type).to_lowercase(),
                    target_chain: ctx.target_chain,
                    started_at: ctx.started_at,
                    last_submission_at,
                    status: "active".to_string(),
                    cpu_threads: ctx.cpu_threads,
                    verified_proof_count,
                    rejected_proof_count,
                    est_net_usd_cents: 0,
                    site_code: ctx.assigned_site.as_ref().map(|s| s.code.clone()),
                    site_city: ctx.assigned_site.as_ref().map(|s| s.city.clone()),
                    paused,
                });
            }
            cursor = next;
            if cursor == 0 {
                break;
            }
        }
        Ok(summaries)
    }

    /// Deletes all keys associated with a session.
    pub async fn delete_session(
        &self,
        wallet: &Address,
        session_id: &SessionId,
    ) -> Result<(), SessionManagerError> {
        let mut conn = self.conn.clone();
        let session_key = format!("session:{wallet}:{}", session_id.0);
        let lookup_key = format!("session_lookup:{}", session_id.0);
        let commit_key = format!("commit:{}", session_id.0);
        let count_key = format!("proof_count:{}", session_id.0);
        let proofs_key = format!("proofs:{}", session_id.0);
        let first_at_key = format!("hashrate_first_at:{}", session_id.0);
        let expected_hashes_key = format!("expected_hashes_sum:{}", session_id.0);
        let activity_key = format!("last_activity:{}", session_id.0);
        let _: () = redis::cmd("DEL")
            .arg(&session_key)
            .arg(&lookup_key)
            .arg(&commit_key)
            .arg(&count_key)
            .arg(&proofs_key)
            .arg(&first_at_key)
            .arg(&expected_hashes_key)
            .arg(&activity_key)
            .query_async(&mut conn)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    // Run with: cargo test -p session-manager -- --ignored
    use super::*;
    use common::ClientType;

    const TEST_URL: &str = "redis://127.0.0.1/";

    fn sample_ctx() -> SessionContext {
        SessionContext {
            wallet: Address::ZERO,
            ip: None,
            client_type: ClientType::Cli,
            active_sessions_count: 0,
            started_at: Utc::now(),
            last_submission_at: None,
            assigned_node_id: None,
            commodity: common::Commodity::Gold,
            target_chain: Chain::Polygon,
            cpu_threads: 8,
            assigned_site: None,
        }
    }

    #[tokio::test]
    #[ignore]
    async fn test_create_and_get_session() {
        let store = SessionStore::new(TEST_URL).await.unwrap();
        let ctx = sample_ctx();
        let id = store.create_session(&ctx).await.unwrap();
        let got = store.get_session(&id).await.unwrap();
        assert_eq!(got, Some(ctx));
        store.delete_session(&Address::ZERO, &id).await.unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn test_commit_and_verify() {
        let store = SessionStore::new(TEST_URL).await.unwrap();
        let id = store.create_session(&sample_ctx()).await.unwrap();
        let nonce = b"primora-nonce";
        let mut hasher = Sha256::new();
        hasher.update(nonce);
        let mut commit = [0u8; 32];
        commit.copy_from_slice(hasher.finalize().as_ref());
        store.set_commit(&id, commit).await.unwrap();
        assert!(store.verify_reveal(&id, nonce).await.unwrap());
        assert!(!store.verify_reveal(&id, b"wrong").await.unwrap());
        store.delete_session(&Address::ZERO, &id).await.unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn test_increment_proof_count() {
        let store = SessionStore::new(TEST_URL).await.unwrap();
        let id = store.create_session(&sample_ctx()).await.unwrap();
        assert_eq!(store.increment_proof_count(&id).await.unwrap(), 1);
        assert_eq!(store.increment_proof_count(&id).await.unwrap(), 2);
        store.delete_session(&Address::ZERO, &id).await.unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn test_get_proof_count_missing() {
        let store = SessionStore::new(TEST_URL).await.unwrap();
        let id = SessionId("nonexistent-session-xyz".to_string());
        assert_eq!(store.get_proof_count(&id).await.unwrap(), 0);
    }

    #[tokio::test]
    #[ignore]
    async fn test_pause_resume_flag() {
        let store = SessionStore::new(TEST_URL).await.unwrap();
        let id = store.create_session(&sample_ctx()).await.unwrap();
        assert!(!store.is_paused(&id).await.unwrap());
        store.set_paused(&id).await.unwrap();
        assert!(store.is_paused(&id).await.unwrap());
        store.set_paused(&id).await.unwrap();
        assert!(store.is_paused(&id).await.unwrap());
        store.clear_paused(&id).await.unwrap();
        assert!(!store.is_paused(&id).await.unwrap());
        store.clear_paused(&id).await.unwrap();
        assert!(!store.is_paused(&id).await.unwrap());
        store.delete_session(&Address::ZERO, &id).await.unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn test_verified_and_rejected_counters() {
        let store = SessionStore::new(TEST_URL).await.unwrap();
        let id = store.create_session(&sample_ctx()).await.unwrap();
        assert_eq!(store.get_verified_proof_count(&id).await.unwrap(), 0);
        assert_eq!(store.get_rejected_proof_count(&id).await.unwrap(), 0);
        assert_eq!(store.increment_verified_proof_count(&id).await.unwrap(), 1);
        assert_eq!(store.increment_verified_proof_count(&id).await.unwrap(), 2);
        assert_eq!(store.increment_rejected_proof_count(&id).await.unwrap(), 1);
        assert_eq!(store.get_verified_proof_count(&id).await.unwrap(), 2);
        assert_eq!(store.get_rejected_proof_count(&id).await.unwrap(), 1);
        store.delete_session(&Address::ZERO, &id).await.unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn test_session_hashrate_missing_is_zero() {
        let store = SessionStore::new(TEST_URL).await.unwrap();
        let id = SessionId("nonexistent-hashrate-xyz".to_string());
        assert_eq!(
            store
                .get_session_hashrate(&id, ClientType::Desktop)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_list_sessions_for_wallet() {
        let store = SessionStore::new(TEST_URL).await.unwrap();
        let ctx = sample_ctx();
        let id = store.create_session(&ctx).await.unwrap();
        let wallet = format!("{}", ctx.wallet);
        let sessions = store.list_sessions_for_wallet(&wallet).await.unwrap();
        let summary = sessions
            .iter()
            .find(|s| s.session_id == id.0)
            .expect("created session present in listing");
        assert_eq!(summary.status, "active");
        assert_eq!(summary.started_at, ctx.started_at);
        assert_eq!(summary.target_chain, Chain::Polygon);
        assert!(summary.last_submission_at.is_none());
        assert_eq!(summary.avg_hashrate, 0);
        assert_eq!(summary.client_type, "cli");

        let before = Utc::now().timestamp();
        store.touch_last_activity(&id).await.unwrap();
        let sessions = store.list_sessions_for_wallet(&wallet).await.unwrap();
        let summary = sessions
            .iter()
            .find(|s| s.session_id == id.0)
            .expect("created session present in listing");
        let activity = summary
            .last_submission_at
            .expect("last activity present after touch");
        assert!((activity.timestamp() - before).abs() <= 5);

        store.delete_session(&Address::ZERO, &id).await.unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn test_session_hashrate_single_proof_is_zero() {
        let store = SessionStore::new(TEST_URL).await.unwrap();
        let id = store.create_session(&sample_ctx()).await.unwrap();
        let now = Utc::now().timestamp();
        store.record_accepted_proof(&id, 30_000, now).await.unwrap();
        store.touch_last_activity(&id).await.unwrap();
        assert_eq!(
            store
                .get_session_hashrate(&id, ClientType::Desktop)
                .await
                .unwrap(),
            0
        );
        store.delete_session(&Address::ZERO, &id).await.unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn test_session_hashrate_derived() {
        let store = SessionStore::new(TEST_URL).await.unwrap();
        let id = store.create_session(&sample_ctx()).await.unwrap();
        let now = Utc::now().timestamp();
        store.record_accepted_proof(&id, 0, now - 100).await.unwrap();
        store.record_accepted_proof(&id, 300_000, now).await.unwrap();
        store.touch_last_activity(&id).await.unwrap();
        let rate = store
            .get_session_hashrate(&id, ClientType::Desktop)
            .await
            .unwrap();
        assert!((2_900..=3_050).contains(&rate), "rate {rate} out of band");
        store.delete_session(&Address::ZERO, &id).await.unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn test_session_hashrate_clamped_to_max() {
        let store = SessionStore::new(TEST_URL).await.unwrap();
        let id = store.create_session(&sample_ctx()).await.unwrap();
        let now = Utc::now().timestamp();
        store.record_accepted_proof(&id, 0, now - 1).await.unwrap();
        store
            .record_accepted_proof(&id, 10_000_000, now)
            .await
            .unwrap();
        store.touch_last_activity(&id).await.unwrap();
        assert_eq!(
            store
                .get_session_hashrate(&id, ClientType::Desktop)
                .await
                .unwrap(),
            4_000
        );
        store.delete_session(&Address::ZERO, &id).await.unwrap();
    }
}
