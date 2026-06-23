#![deny(warnings)]
#![deny(missing_docs)]
//! PostgreSQL persistence for anomaly events and mint proposals.

use chrono::{DateTime, Utc};
use common::{AnomalyEvent, MintProposal, ProposalStatus, SessionId};
use serde::Serialize;
use sqlx::Row;

/// Errors returned by the Postgres store.
#[derive(Debug)]
pub enum PostgresStoreError {
    /// Database transport or query error.
    Sqlx(sqlx::Error),
    /// JSON serialization error while encoding a column value.
    Serialization(serde_json::Error),
    /// Migration execution error.
    Migration(sqlx::migrate::MigrateError),
}

impl std::fmt::Display for PostgresStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlx(e) => write!(f, "database error: {e}"),
            Self::Serialization(e) => write!(f, "serialization error: {e}"),
            Self::Migration(e) => write!(f, "migration error: {e}"),
        }
    }
}

impl std::error::Error for PostgresStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlx(e) => Some(e),
            Self::Serialization(e) => Some(e),
            Self::Migration(e) => Some(e),
        }
    }
}

impl From<sqlx::Error> for PostgresStoreError {
    fn from(e: sqlx::Error) -> Self {
        Self::Sqlx(e)
    }
}

impl From<serde_json::Error> for PostgresStoreError {
    fn from(e: serde_json::Error) -> Self {
        Self::Serialization(e)
    }
}

impl From<sqlx::migrate::MigrateError> for PostgresStoreError {
    fn from(e: sqlx::migrate::MigrateError) -> Self {
        Self::Migration(e)
    }
}

/// A pending mint proposal row projected for the admin panel queue.
#[derive(Debug, Clone, Serialize)]
pub struct PendingProposalRow {
    /// Database row identifier.
    pub id: uuid::Uuid,
    /// Source session identifier.
    pub session_id: String,
    /// Recipient wallet, debug-formatted address string.
    pub wallet: String,
    /// Gross PRM as a decimal string (NUMERIC rendered as text).
    pub gross_prm: String,
    /// Backing commodity name.
    pub commodity: String,
    /// Current proposal status.
    pub status: String,
    /// Proposal creation time.
    pub created_at: DateTime<Utc>,
}

/// Postgres-backed store for anomaly events and mint proposals.
pub struct PostgresStore {
    pool: sqlx::PgPool,
}

impl PostgresStore {
    /// Connects to the database at `database_url` and returns a store over the
    /// resulting connection pool.
    pub async fn new(database_url: &str) -> Result<Self, PostgresStoreError> {
        let pool = sqlx::PgPool::connect(database_url).await?;
        Ok(Self { pool })
    }

    /// Applies all embedded migrations to the connected database.
    pub async fn run_migrations(&self) -> Result<(), PostgresStoreError> {
        sqlx::migrate!("./migrations").run(&self.pool).await?;
        Ok(())
    }

    /// Persists an anomaly event.
    pub async fn insert_anomaly_event(
        &self,
        event: &AnomalyEvent,
    ) -> Result<(), PostgresStoreError> {
        let triggers = serde_json::to_value(&event.triggers)?;
        sqlx::query(
            "INSERT INTO anomaly_events \
             (session_id, wallet, score, triggers, level, created_at) \
             VALUES ($1, $2, $3, $4::jsonb, $5, $6)",
        )
        .bind(event.session_id.0.as_str())
        .bind(format!("{:?}", event.wallet))
        .bind(event.score as i32)
        .bind(triggers)
        .bind(format!("{:?}", event.level))
        .bind(event.timestamp)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Persists a signed mint proposal.
    pub async fn insert_mint_proposal(
        &self,
        proposal: &MintProposal,
    ) -> Result<(), PostgresStoreError> {
        let attestation_json = serde_json::to_value(&proposal.attestation)?;
        sqlx::query(
            "INSERT INTO mint_proposals \
             (session_id, wallet, gross_prm, commodity, attestation_json, backend_sig, status, created_at) \
             VALUES ($1, $2, $3::numeric, $4, $5::jsonb, $6, $7, $8)",
        )
        .bind(proposal.session_id.0.as_str())
        .bind(format!("{:?}", proposal.wallet))
        .bind(proposal.gross_prm.to_string())
        .bind(format!("{:?}", proposal.commodity))
        .bind(attestation_json)
        .bind(format!("{}", proposal.backend_sig))
        .bind(format!("{:?}", proposal.status))
        .bind(proposal.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Updates the lifecycle status of the proposal for `session_id`.
    pub async fn update_proposal_status(
        &self,
        session_id: &SessionId,
        status: ProposalStatus,
    ) -> Result<(), PostgresStoreError> {
        sqlx::query("UPDATE mint_proposals SET status = $1, updated_at = NOW() WHERE session_id = $2")
            .bind(format!("{status:?}"))
            .bind(session_id.0.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Returns all pending proposals ordered oldest first.
    pub async fn get_pending_proposals(
        &self,
    ) -> Result<Vec<PendingProposalRow>, PostgresStoreError> {
        let rows = sqlx::query(
            "SELECT id, session_id, wallet, gross_prm::text AS gross_prm, commodity, status, created_at \
             FROM mint_proposals WHERE status = 'Pending' ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut proposals = Vec::with_capacity(rows.len());
        for row in rows {
            proposals.push(PendingProposalRow {
                id: row.try_get("id")?,
                session_id: row.try_get("session_id")?,
                wallet: row.try_get("wallet")?,
                gross_prm: row.try_get("gross_prm")?,
                commodity: row.try_get("commodity")?,
                status: row.try_get("status")?,
                created_at: row.try_get("created_at")?,
            });
        }
        Ok(proposals)
    }
}

#[cfg(test)]
mod tests {
    // Run with: cargo test -p postgres-store -- --ignored
    use super::*;
    use alloy_primitives::{Address, Signature, U256};
    use common::{AttestationResult, Commodity, InvalidReason, SuspicionLevel};

    const TEST_DB: &str = "postgres://postgres:postgres@localhost/primora_test";

    fn epoch() -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap()
    }

    async fn store() -> PostgresStore {
        let store = PostgresStore::new(TEST_DB).await.unwrap();
        store.run_migrations().await.unwrap();
        store
    }

    fn dummy_anomaly() -> AnomalyEvent {
        AnomalyEvent {
            session_id: SessionId("sess-anomaly".to_string()),
            wallet: Address::ZERO,
            score: 0,
            triggers: vec![InvalidReason::TimingAnomaly],
            level: SuspicionLevel::Medium,
            timestamp: epoch(),
        }
    }

    fn dummy_proposal(session_id: &str) -> MintProposal {
        MintProposal {
            session_id: SessionId(session_id.to_string()),
            wallet: Address::ZERO,
            gross_prm: 18_000_000_000_000_000_000_000,
            commodity: Commodity::Gold,
            attestation: AttestationResult {
                session_id: SessionId(session_id.to_string()),
                signatures: Vec::new(),
                node_ids: Vec::new(),
                proof_hash: [0u8; 32],
                timestamp: epoch(),
            },
            backend_sig: Signature::new(U256::ZERO, U256::ZERO, false),
            created_at: epoch(),
            status: ProposalStatus::Pending,
        }
    }

    #[tokio::test]
    #[ignore]
    async fn test_insert_anomaly_event() {
        let store = store().await;
        store.insert_anomaly_event(&dummy_anomaly()).await.unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn test_insert_mint_proposal() {
        let store = store().await;
        store
            .insert_mint_proposal(&dummy_proposal("sess-insert"))
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn test_update_proposal_status() {
        let store = store().await;
        let session_id = SessionId("sess-update".to_string());
        store
            .insert_mint_proposal(&dummy_proposal("sess-update"))
            .await
            .unwrap();
        store
            .update_proposal_status(&session_id, ProposalStatus::ApprovedByMultiSig)
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn test_get_pending_proposals() {
        let store = store().await;
        store
            .insert_mint_proposal(&dummy_proposal("sess-pending"))
            .await
            .unwrap();
        let pending = store.get_pending_proposals().await.unwrap();
        assert!(pending.iter().all(|row| row.status == "Pending"));
    }
}
