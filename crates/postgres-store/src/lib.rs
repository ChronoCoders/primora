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
    /// The chain this proposal mints to.
    pub chain: String,
    /// Current proposal status.
    pub status: String,
    /// Proposal creation time.
    pub created_at: DateTime<Utc>,
}

/// A payout (mint proposal) row projected for a wallet's payout history.
#[derive(Debug, Clone, Serialize)]
pub struct PayoutRow {
    /// Source session identifier.
    pub session_id: String,
    /// Recipient wallet, debug-formatted address string.
    pub wallet: String,
    /// Gross PRM as a decimal string (NUMERIC rendered as text).
    pub gross_prm: String,
    /// Net payout in USD cents (Spec 4.8). `None` for rows that predate USD
    /// persistence; the frontend renders a dash for these.
    pub net_usd_cents: Option<i64>,
    /// Backing commodity name.
    pub commodity: String,
    /// The chain this proposal mints to.
    pub chain: String,
    /// Current proposal status.
    pub status: String,
    /// Proposal creation time.
    pub created_at: DateTime<Utc>,
}

/// Aggregated earnings for a single commodity for one wallet.
#[derive(Debug, Clone, Serialize)]
pub struct EarningsRow {
    /// Backing commodity name.
    pub commodity: String,
    /// Number of proposals (sessions) recorded for this commodity.
    pub session_count: i64,
    /// Summed minted PRM in ERC-20 base-unit wei (NUMERIC SUM rendered as text);
    /// human PRM = value / 10^18.
    pub total_gross_prm: String,
    /// Net redemption USD earned, in cents: the sum of per-payout `net_usd_cents`
    /// (TWAP redemption minus house edge, Spec 4.6). Rows predating the
    /// `net_usd_cents` column (migration 0004) contribute 0.
    pub total_usd_cents: i64,
}

/// A wallet's total earnings over the last 24 hours.
#[derive(Debug, Clone, Serialize)]
pub struct Earnings24h {
    /// Total gross PRM minted in the last 24h, base-unit wei as a decimal string
    /// (NUMERIC SUM rendered as text); human PRM = value / 10^18.
    pub total_gross_prm: String,
    /// Total net redemption USD in the last 24h, in cents (sum of per-payout
    /// `net_usd_cents`, after the house edge; NULLs counted as 0).
    pub total_usd_cents: i64,
    /// Number of payouts in the 24h window.
    pub payout_count: i64,
}

/// Cumulative, confirmed-only mining totals backing the Company Mining Share KPI
/// (Spec §12). All-time and cross-chain; `Pending`/`Submitted` proposals are
/// excluded so only PRM actually minted on-chain counts.
#[derive(Debug, Clone, Serialize)]
pub struct EntityShare {
    /// Confirmed PRM minted to the company wallet, base-unit wei (decimal string).
    pub company_prm_wei: String,
    /// Confirmed PRM minted to all wallets, base-unit wei (decimal string).
    pub total_prm_wei: String,
    /// Company share of confirmed mining in basis points (0..=10000); 0 when no
    /// confirmed mints exist yet (divide-by-zero guarded in the query).
    pub share_bps: i64,
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
             (session_id, wallet, gross_prm, net_usd_cents, commodity, chain, attestation_json, backend_sig, status, created_at) \
             VALUES ($1, $2, $3::numeric, $4, $5, $6, $7::jsonb, $8, $9, $10)",
        )
        .bind(proposal.session_id.0.as_str())
        .bind(format!("{:?}", proposal.wallet))
        .bind(proposal.gross_prm.to_string())
        .bind(proposal.net_usd_cents)
        .bind(format!("{:?}", proposal.commodity))
        .bind(proposal.chain.as_str())
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
            "SELECT id, session_id, wallet, gross_prm::text AS gross_prm, commodity, chain, status, created_at \
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
                chain: row.try_get("chain")?,
                status: row.try_get("status")?,
                created_at: row.try_get("created_at")?,
            });
        }
        Ok(proposals)
    }

    /// Returns a wallet's payout history (mint proposals) newest first, capped at
    /// `limit`. `wallet` must already be debug-formatted (`format!("{:?}", _)`)
    /// to match stored rows. Gross PRM is cast to text to preserve full NUMERIC
    /// precision without floating point.
    pub async fn get_payouts_for_wallet(
        &self,
        wallet: &str,
        limit: i64,
    ) -> Result<Vec<PayoutRow>, PostgresStoreError> {
        let rows = sqlx::query(
            "SELECT session_id, wallet, gross_prm::text AS gross_prm, net_usd_cents, commodity, chain, status, created_at \
             FROM mint_proposals WHERE wallet = $1 ORDER BY created_at DESC LIMIT $2",
        )
        .bind(wallet)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        let mut payouts = Vec::with_capacity(rows.len());
        for row in rows {
            payouts.push(PayoutRow {
                session_id: row.try_get("session_id")?,
                wallet: row.try_get("wallet")?,
                gross_prm: row.try_get("gross_prm")?,
                net_usd_cents: row.try_get("net_usd_cents")?,
                commodity: row.try_get("commodity")?,
                chain: row.try_get("chain")?,
                status: row.try_get("status")?,
                created_at: row.try_get("created_at")?,
            });
        }
        Ok(payouts)
    }

    /// Returns a wallet's earnings aggregated by commodity. `wallet` must already
    /// be debug-formatted (`format!("{:?}", _)`) to match stored rows. The gross
    /// PRM (wei) sum is cast to text to preserve full NUMERIC precision without
    /// floating point; `total_usd_cents` is the summed net redemption USD
    /// (`SUM(net_usd_cents)`, after the house edge), with NULLs counted as 0.
    /// Earnings are aggregated across all chains (not split by chain), matching
    /// the Overview mock's commodity-only breakdown.
    pub async fn get_earnings_by_commodity(
        &self,
        wallet: &str,
    ) -> Result<Vec<EarningsRow>, PostgresStoreError> {
        let rows = sqlx::query(
            "SELECT commodity, COUNT(*) AS session_count, \
             COALESCE(SUM(gross_prm), 0)::text AS total_gross_prm, \
             COALESCE(SUM(net_usd_cents), 0)::bigint AS total_usd_cents \
             FROM mint_proposals WHERE wallet = $1 GROUP BY commodity",
        )
        .bind(wallet)
        .fetch_all(&self.pool)
        .await?;

        let mut earnings = Vec::with_capacity(rows.len());
        for row in rows {
            earnings.push(EarningsRow {
                commodity: row.try_get("commodity")?,
                session_count: row.try_get("session_count")?,
                total_gross_prm: row.try_get("total_gross_prm")?,
                total_usd_cents: row.try_get("total_usd_cents")?,
            });
        }
        Ok(earnings)
    }

    /// Returns the total `gross_prm` (wei) across ALL mint proposals created in
    /// the last 24 hours, as a decimal string preserving full NUMERIC precision.
    /// Backs the backend per-day mint ceiling. `"0"` when there are none.
    pub async fn total_minted_wei_24h(&self) -> Result<String, PostgresStoreError> {
        let row = sqlx::query(
            "SELECT COALESCE(SUM(gross_prm), 0)::text AS total \
             FROM mint_proposals \
             WHERE created_at > now() - interval '24 hours'",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row.try_get("total")?)
    }

    /// Returns a wallet's total earnings over the last 24 hours: summed gross PRM
    /// (wei) and net redemption USD (cents). `wallet` must be debug-formatted
    /// (`format!("{:?}", _)`) to match stored rows, identical to
    /// [`PostgresStore::get_earnings_by_commodity`]. The window is
    /// `created_at > now() - interval '24 hours'`; the gross sum is cast to text
    /// to preserve full NUMERIC precision without floating point, and
    /// `net_usd_cents` NULLs (pre-migration rows) count as 0.
    pub async fn get_earnings_24h(&self, wallet: &str) -> Result<Earnings24h, PostgresStoreError> {
        let row = sqlx::query(
            "SELECT COALESCE(SUM(gross_prm), 0)::text AS total_gross_prm, \
             COALESCE(SUM(net_usd_cents), 0)::bigint AS total_usd_cents, \
             COUNT(*)::bigint AS payout_count \
             FROM mint_proposals \
             WHERE wallet = $1 AND created_at > now() - interval '24 hours'",
        )
        .bind(wallet)
        .fetch_one(&self.pool)
        .await?;
        Ok(Earnings24h {
            total_gross_prm: row.try_get("total_gross_prm")?,
            total_usd_cents: row.try_get("total_usd_cents")?,
            payout_count: row.try_get("payout_count")?,
        })
    }

    /// Returns the cumulative, confirmed-only Company Mining Share (Spec §12):
    /// company-wallet confirmed PRM over total confirmed PRM, all-time and across
    /// every chain (one row per mint, no chain filter, no double-count).
    /// `company_wallet_key` must be debug-formatted (`format!("{:?}", _)`) to
    /// match stored `mint_proposals.wallet`. Only `status = 'Confirmed'` rows
    /// count (actually minted on-chain); never-minted `Pending`/`Submitted`
    /// proposals are excluded. `share_bps` is computed in arbitrary-precision
    /// NUMERIC and divide-by-zero guarded (0 when no confirmed mints exist).
    pub async fn company_mining_share(
        &self,
        company_wallet_key: &str,
    ) -> Result<EntityShare, PostgresStoreError> {
        let row = sqlx::query(
            "SELECT \
             COALESCE(SUM(gross_prm) FILTER (WHERE wallet = $1), 0)::text AS company, \
             COALESCE(SUM(gross_prm), 0)::text AS total, \
             COALESCE( \
               SUM(gross_prm) FILTER (WHERE wallet = $1) * 10000 / NULLIF(SUM(gross_prm), 0), \
               0 \
             )::bigint AS share_bps \
             FROM mint_proposals \
             WHERE status = 'Confirmed'",
        )
        .bind(company_wallet_key)
        .fetch_one(&self.pool)
        .await?;
        Ok(EntityShare {
            company_prm_wei: row.try_get("company")?,
            total_prm_wei: row.try_get("total")?,
            share_bps: row.try_get("share_bps")?,
        })
    }
}

#[cfg(test)]
mod tests {
    // Run with: cargo test -p postgres-store -- --ignored
    use super::*;
    use alloy_primitives::{Address, Signature, U256};
    use common::{AttestationResult, Chain, Commodity, InvalidReason, SuspicionLevel};

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
            score: 2_500,
            triggers: vec![InvalidReason::TimingAnomaly],
            level: SuspicionLevel::Medium,
            timestamp: epoch(),
        }
    }

    fn dummy_proposal(session_id: &str, chain: Chain) -> MintProposal {
        MintProposal {
            session_id: SessionId(session_id.to_string()),
            wallet: Address::ZERO,
            gross_prm: 18_000_000_000_000_000_000_000,
            net_usd_cents: Some(1_531),
            commodity: Commodity::Gold,
            chain,
            attestation: AttestationResult {
                session_id: SessionId(session_id.to_string()),
                signatures: Vec::new(),
                node_ids: Vec::new(),
                signers: Vec::new(),
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
            .insert_mint_proposal(&dummy_proposal("sess-insert", Chain::Ethereum))
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn test_update_proposal_status() {
        let store = store().await;
        let session_id = SessionId("sess-update".to_string());
        store
            .insert_mint_proposal(&dummy_proposal("sess-update", Chain::Ethereum))
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
            .insert_mint_proposal(&dummy_proposal("sess-pending", Chain::Ethereum))
            .await
            .unwrap();
        let pending = store.get_pending_proposals().await.unwrap();
        assert!(pending.iter().all(|row| row.status == "Pending"));
        assert!(pending.iter().all(|row| row.chain == "ethereum" || row.chain == "polygon"));
    }

    #[tokio::test]
    #[ignore]
    async fn test_get_payouts_for_wallet() {
        let store = store().await;
        store
            .insert_mint_proposal(&dummy_proposal("sess-payout", Chain::Polygon))
            .await
            .unwrap();
        let wallet = format!("{:?}", Address::ZERO);
        let payouts = store.get_payouts_for_wallet(&wallet, 50).await.unwrap();
        let row = payouts
            .iter()
            .find(|p| p.session_id == "sess-payout")
            .expect("inserted payout present");
        assert_eq!(row.wallet, wallet);
        assert_eq!(row.chain, "polygon");
        assert!(row.gross_prm.parse::<u128>().is_ok());
        assert_eq!(row.net_usd_cents, Some(1_531));
    }

    #[tokio::test]
    #[ignore]
    async fn test_payout_chain_per_row() {
        let store = store().await;
        store
            .insert_mint_proposal(&dummy_proposal("sess-eth", Chain::Ethereum))
            .await
            .unwrap();
        store
            .insert_mint_proposal(&dummy_proposal("sess-pol", Chain::Polygon))
            .await
            .unwrap();
        let wallet = format!("{:?}", Address::ZERO);
        let payouts = store.get_payouts_for_wallet(&wallet, 50).await.unwrap();
        let eth = payouts
            .iter()
            .find(|p| p.session_id == "sess-eth")
            .expect("ethereum proposal present");
        let pol = payouts
            .iter()
            .find(|p| p.session_id == "sess-pol")
            .expect("polygon proposal present");
        assert_eq!(eth.chain, "ethereum");
        assert_eq!(pol.chain, "polygon");
    }

    #[tokio::test]
    #[ignore]
    async fn test_get_earnings_by_commodity() {
        let store = store().await;
        let wallet_addr = Address::from([0xEEu8; 20]);
        let mut a = dummy_proposal("sess-earn-a", Chain::Ethereum);
        a.wallet = wallet_addr;
        a.net_usd_cents = Some(1_531);
        let mut b = dummy_proposal("sess-earn-b", Chain::Polygon);
        b.wallet = wallet_addr;
        b.net_usd_cents = Some(469);
        store.insert_mint_proposal(&a).await.unwrap();
        store.insert_mint_proposal(&b).await.unwrap();

        let wallet = format!("{:?}", wallet_addr);
        let earnings = store.get_earnings_by_commodity(&wallet).await.unwrap();
        let gold = earnings
            .iter()
            .find(|e| e.commodity == "Gold")
            .expect("gold earnings present");
        assert_eq!(gold.session_count, 2);
        assert!(gold.total_gross_prm.parse::<u128>().is_ok());
        assert_eq!(gold.total_usd_cents, 2_000);
    }

    #[tokio::test]
    #[ignore]
    async fn test_get_earnings_24h_window() {
        let store = store().await;
        let wallet_addr = Address::from([0xABu8; 20]);
        let mut recent = dummy_proposal("sess-24h-recent", Chain::Ethereum);
        recent.wallet = wallet_addr;
        recent.net_usd_cents = Some(1_531);
        recent.created_at = Utc::now();
        let mut old = dummy_proposal("sess-24h-old", Chain::Polygon);
        old.wallet = wallet_addr;
        old.net_usd_cents = Some(999);
        old.created_at = Utc::now() - chrono::Duration::days(2);
        store.insert_mint_proposal(&recent).await.unwrap();
        store.insert_mint_proposal(&old).await.unwrap();

        let wallet = format!("{:?}", wallet_addr);
        let earnings = store.get_earnings_24h(&wallet).await.unwrap();
        assert_eq!(earnings.payout_count, 1);
        assert_eq!(earnings.total_usd_cents, 1_531);
        assert_eq!(earnings.total_gross_prm, "18000000000000000000000");
    }

    #[tokio::test]
    #[ignore]
    async fn test_total_minted_wei_24h_window() {
        let store = store().await;
        let before: u128 = store.total_minted_wei_24h().await.unwrap().parse().unwrap();
        let mut recent_a = dummy_proposal("sess-mint-recent-a", Chain::Ethereum);
        recent_a.created_at = Utc::now();
        let mut recent_b = dummy_proposal("sess-mint-recent-b", Chain::Polygon);
        recent_b.created_at = Utc::now();
        let mut old = dummy_proposal("sess-mint-old", Chain::Ethereum);
        old.created_at = Utc::now() - chrono::Duration::days(2);
        store.insert_mint_proposal(&recent_a).await.unwrap();
        store.insert_mint_proposal(&recent_b).await.unwrap();
        store.insert_mint_proposal(&old).await.unwrap();

        let after: u128 = store.total_minted_wei_24h().await.unwrap().parse().unwrap();
        assert_eq!(after - before, 2 * 18_000_000_000_000_000_000_000u128);
    }

    async fn seed_proposal(
        store: &PostgresStore,
        sid: &str,
        wallet: Address,
        gross_prm: u128,
        chain: Chain,
        status: ProposalStatus,
    ) {
        let mut p = dummy_proposal(sid, chain);
        p.wallet = wallet;
        p.gross_prm = gross_prm;
        store.insert_mint_proposal(&p).await.unwrap();
        if status != ProposalStatus::Pending {
            store
                .update_proposal_status(&SessionId(sid.to_string()), status)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    #[ignore]
    async fn test_company_mining_share_confirmed_only() {
        let store = store().await;
        // Company Mining Share is a global, all-time ratio; clear prior rows so the
        // seeded company-vs-user split is the entire confirmed set under assertion.
        sqlx::query("TRUNCATE mint_proposals")
            .execute(&store.pool)
            .await
            .unwrap();
        let company = Address::from([0x11u8; 20]);
        let user = Address::from([0x22u8; 20]);

        // Company confirmed = 30 (ETH) + 10 (POL) = 40, summed cross-chain.
        seed_proposal(&store, "ems-co-eth", company, 30, Chain::Ethereum, ProposalStatus::Confirmed).await;
        seed_proposal(&store, "ems-co-pol", company, 10, Chain::Polygon, ProposalStatus::Confirmed).await;
        // User confirmed = 10 (ETH). Total confirmed = 50.
        seed_proposal(&store, "ems-user-eth", user, 10, Chain::Ethereum, ProposalStatus::Confirmed).await;
        // Company PENDING = 100: must be EXCLUDED (would skew to 140/150 if counted).
        seed_proposal(&store, "ems-co-pending", company, 100, Chain::Ethereum, ProposalStatus::Pending).await;

        let key = format!("{:?}", company);
        let share = store.company_mining_share(&key).await.unwrap();
        assert_eq!(share.company_prm_wei, "40");
        assert_eq!(share.total_prm_wei, "50");
        assert_eq!(share.share_bps, 8_000);

        // No company wallet configured: company share 0, total still computed.
        let none = store.company_mining_share("").await.unwrap();
        assert_eq!(none.company_prm_wei, "0");
        assert_eq!(none.total_prm_wei, "50");
        assert_eq!(none.share_bps, 0);
    }
}
