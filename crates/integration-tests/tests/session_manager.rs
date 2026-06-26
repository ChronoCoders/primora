use alloy_primitives::Address;
use common::{Chain, ClientType, Commodity, SessionContext};
use session_manager::SessionStore;
use sha2::{Digest, Sha256};

async fn connect() -> Option<SessionStore> {
    let Ok(url) = std::env::var("REDIS_URL") else {
        eprintln!("REDIS_URL not set, skipping");
        return None;
    };
    let Ok(store) = SessionStore::new(&url).await else {
        panic!("failed to connect to Redis at REDIS_URL");
    };
    Some(store)
}

fn sample_ctx() -> SessionContext {
    SessionContext {
        wallet: Address::ZERO,
        ip: None,
        client_type: ClientType::Browser,
        active_sessions_count: 0,
        started_at: chrono::Utc::now(),
        last_submission_at: None,
        recent_proof_count: 0,
        assigned_node_id: None,
        commodity: Commodity::Gold,
        target_chain: Chain::Ethereum,
    }
}

#[tokio::test]
async fn test_create_and_get_session() {
    let Some(store) = connect().await else {
        return;
    };
    let ctx = sample_ctx();
    let Ok(session_id) = store.create_session(&ctx).await else {
        panic!("create_session failed");
    };
    let Ok(got) = store.get_session(&session_id).await else {
        panic!("get_session failed");
    };
    assert_eq!(got, Some(ctx));
    assert!(store.delete_session(&Address::ZERO, &session_id).await.is_ok());
}

#[tokio::test]
async fn test_commit_reveal_flow() {
    let Some(store) = connect().await else {
        return;
    };
    let Ok(session_id) = store.create_session(&sample_ctx()).await else {
        panic!("create_session failed");
    };
    let nonce = b"test-nonce-12345";
    let digest = Sha256::digest(nonce);
    let mut commit_hash = [0u8; 32];
    commit_hash.copy_from_slice(&digest);
    assert!(store.set_commit(&session_id, commit_hash).await.is_ok());

    let Ok(matches) = store.verify_reveal(&session_id, nonce).await else {
        panic!("verify_reveal failed");
    };
    assert!(matches);
    let Ok(rejects) = store.verify_reveal(&session_id, b"wrong-nonce").await else {
        panic!("verify_reveal failed");
    };
    assert!(!rejects);

    assert!(store.delete_session(&Address::ZERO, &session_id).await.is_ok());
}

#[tokio::test]
async fn test_proof_count_increment() {
    let Some(store) = connect().await else {
        return;
    };
    let Ok(session_id) = store.create_session(&sample_ctx()).await else {
        panic!("create_session failed");
    };
    for expected in 1..=3u32 {
        let Ok(count) = store.increment_proof_count(&session_id).await else {
            panic!("increment_proof_count failed");
        };
        assert_eq!(count, expected);
    }
    assert!(store.delete_session(&Address::ZERO, &session_id).await.is_ok());
}
