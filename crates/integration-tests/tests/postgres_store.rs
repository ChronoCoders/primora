use alloy_primitives::{Address, Signature, U256};
use chrono::Utc;
use common::{
    AnomalyEvent, AttestationResult, Chain, Commodity, InvalidReason, MintProposal, ProposalStatus,
    SessionId, SuspicionLevel,
};
use postgres_store::PostgresStore;

async fn connect() -> Option<PostgresStore> {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL not set, skipping");
        return None;
    };
    let Ok(store) = PostgresStore::new(&url).await else {
        panic!("failed to connect to Postgres at DATABASE_URL");
    };
    let Ok(()) = store.run_migrations().await else {
        panic!("run_migrations failed");
    };
    Some(store)
}

fn unique_session_id() -> SessionId {
    SessionId(uuid::Uuid::new_v4().to_string())
}

#[tokio::test]
async fn test_insert_and_query_anomaly_event() {
    let Some(store) = connect().await else {
        return;
    };
    let event = AnomalyEvent {
        session_id: unique_session_id(),
        wallet: Address::ZERO,
        score: 0,
        triggers: vec![InvalidReason::TimingAnomaly],
        level: SuspicionLevel::Medium,
        timestamp: Utc::now(),
    };
    assert!(store.insert_anomaly_event(&event).await.is_ok());
}

#[tokio::test]
async fn test_insert_and_update_mint_proposal() {
    let Some(store) = connect().await else {
        return;
    };
    let session_id = unique_session_id();
    let proposal = MintProposal {
        session_id: session_id.clone(),
        wallet: Address::ZERO,
        gross_prm: 1_000_000_000_000_000_000u128,
        net_usd_cents: Some(1_531),
        commodity: Commodity::Gold,
        chain: Chain::Ethereum,
        attestation: AttestationResult {
            session_id: session_id.clone(),
            signatures: Vec::new(),
            node_ids: Vec::new(),
            signers: Vec::new(),
            proof_hash: [0u8; 32],
            timestamp: Utc::now(),
        },
        backend_sig: Signature::new(U256::ZERO, U256::ZERO, false),
        created_at: Utc::now(),
        status: ProposalStatus::Pending,
    };
    assert!(store.insert_mint_proposal(&proposal).await.is_ok());
    assert!(store
        .update_proposal_status(&session_id, ProposalStatus::ApprovedByMultiSig)
        .await
        .is_ok());

    let Ok(pending) = store.get_pending_proposals().await else {
        panic!("get_pending_proposals failed");
    };
    assert!(!pending.iter().any(|row| row.session_id == session_id.0));
}
