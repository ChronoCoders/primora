#![deny(warnings)]
#![deny(missing_docs)]
//! Redis-backed commit-reveal and session lifecycle management.

use alloy_primitives::Address;
use common::{PartialProof, SessionContext, SessionId};
use redis::aio::MultiplexedConnection;
use sha2::{Digest, Sha256};

const TTL_SECS: i64 = 3600;
const COMMIT_PENDING: &str = "pending";

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
        let _: () = redis::cmd("DEL")
            .arg(&session_key)
            .arg(&lookup_key)
            .arg(&commit_key)
            .arg(&count_key)
            .arg(&proofs_key)
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
            last_submission_at: None,
            recent_proof_count: 0,
            assigned_node_id: None,
            commodity: common::Commodity::Gold,
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
}
