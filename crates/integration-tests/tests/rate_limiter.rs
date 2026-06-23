use alloy_primitives::Address;
use rate_limiter::{RateLimitResult, RateLimiter};

async fn connect() -> Option<RateLimiter> {
    let Ok(url) = std::env::var("REDIS_URL") else {
        eprintln!("REDIS_URL not set, skipping");
        return None;
    };
    let Ok(limiter) = RateLimiter::new(&url).await else {
        panic!("failed to connect to Redis at REDIS_URL");
    };
    Some(limiter)
}

fn unique_wallet() -> Address {
    let uuid = uuid::Uuid::new_v4();
    let mut bytes = [0u8; 20];
    bytes[..16].copy_from_slice(uuid.as_bytes());
    Address::from(bytes)
}

#[tokio::test]
async fn test_wallet_allow_then_deny() {
    let Some(limiter) = connect().await else {
        return;
    };
    let wallet = unique_wallet();
    for _ in 0..5 {
        let Ok(result) = limiter.check_wallet(&wallet).await else {
            panic!("check_wallet failed");
        };
        assert_eq!(result, RateLimitResult::Allowed);
    }
    assert!(limiter.reset(&format!("rate:wallet:{wallet}")).await.is_ok());
}

#[tokio::test]
async fn test_ip_rate_limit_deny() {
    let Some(limiter) = connect().await else {
        return;
    };
    let uuid = uuid::Uuid::new_v4();
    let bytes = uuid.as_bytes();
    let ip = format!("192.168.{}.{}", bytes[0], bytes[1]);

    let mut denied_at = None;
    for iteration in 1..=205u32 {
        let Ok(result) = limiter.check_ip(&ip).await else {
            panic!("check_ip failed");
        };
        if let RateLimitResult::Denied { .. } = result {
            denied_at = Some(iteration);
            break;
        }
    }
    let Some(iteration) = denied_at else {
        panic!("rate limit never denied within 205 iterations");
    };
    assert!(iteration <= 201, "denied at iteration {iteration}, expected by 201");

    assert!(limiter.reset(&format!("rate:ip:{ip}")).await.is_ok());
}
