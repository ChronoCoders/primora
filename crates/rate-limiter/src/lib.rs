#![deny(warnings)]
#![deny(missing_docs)]
//! Per-wallet, per-IP, and per-node rate limiting.

use alloy_primitives::Address;
use common::NodeId;
use redis::aio::MultiplexedConnection;

const WALLET_LIMIT: u32 = 100;
const WALLET_WINDOW_SECS: u64 = 86_400;
const IP_LIMIT: u32 = 200;
const IP_WINDOW_SECS: u64 = 3_600;
const NODE_LIMIT: u32 = 1_000;
const NODE_WINDOW_SECS: u64 = 60;

/// Outcome of a rate limit check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitResult {
    /// Request is within the limit.
    Allowed,
    /// Request exceeded the limit for the window.
    Denied {
        /// Maximum allowed requests in the window.
        limit: u32,
        /// Window length in seconds.
        window_secs: u64,
    },
}

/// Errors returned by the rate limiter.
#[derive(Debug)]
pub enum RateLimiterError {
    /// Redis transport or command error.
    Redis(redis::RedisError),
}

impl std::fmt::Display for RateLimiterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Redis(e) => write!(f, "redis error: {e}"),
        }
    }
}

impl std::error::Error for RateLimiterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Redis(e) => Some(e),
        }
    }
}

impl From<redis::RedisError> for RateLimiterError {
    fn from(e: redis::RedisError) -> Self {
        Self::Redis(e)
    }
}

/// Redis-backed fixed-window rate limiter for wallets, IPs, and nodes.
pub struct RateLimiter {
    conn: MultiplexedConnection,
}

impl RateLimiter {
    /// Opens a multiplexed async connection to the Redis instance at `url`.
    pub async fn new(url: &str) -> Result<Self, RateLimiterError> {
        let client = redis::Client::open(url)?;
        let conn = client.get_multiplexed_async_connection().await?;
        Ok(Self { conn })
    }

    async fn check(
        &self,
        key: String,
        limit: u32,
        window_secs: u64,
    ) -> Result<RateLimitResult, RateLimiterError> {
        let mut conn = self.conn.clone();
        let count: i64 = redis::cmd("INCR").arg(&key).query_async(&mut conn).await?;
        if count == 1 {
            let _: () = redis::cmd("EXPIRE")
                .arg(&key)
                .arg(window_secs)
                .query_async(&mut conn)
                .await?;
        }
        if count > i64::from(limit) {
            Ok(RateLimitResult::Denied { limit, window_secs })
        } else {
            Ok(RateLimitResult::Allowed)
        }
    }

    /// Checks the daily request limit for a wallet.
    pub async fn check_wallet(
        &self,
        wallet: &Address,
    ) -> Result<RateLimitResult, RateLimiterError> {
        self.check(format!("rate:wallet:{wallet}"), WALLET_LIMIT, WALLET_WINDOW_SECS)
            .await
    }

    /// Checks the hourly request limit for an IP address.
    pub async fn check_ip(&self, ip: &str) -> Result<RateLimitResult, RateLimiterError> {
        self.check(format!("rate:ip:{ip}"), IP_LIMIT, IP_WINDOW_SECS)
            .await
    }

    /// Checks the per-minute submission limit for a node.
    pub async fn check_node(
        &self,
        node_id: &NodeId,
    ) -> Result<RateLimitResult, RateLimiterError> {
        self.check(format!("rate:node:{}", node_id.0), NODE_LIMIT, NODE_WINDOW_SECS)
            .await
    }

    /// Deletes a rate limit key. Use in tests and admin operations only.
    pub async fn reset(&self, key: &str) -> Result<(), RateLimiterError> {
        let mut conn = self.conn.clone();
        let _: () = redis::cmd("DEL").arg(key).query_async(&mut conn).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    // Run with: cargo test -p rate-limiter -- --ignored
    use super::*;

    const TEST_URL: &str = "redis://127.0.0.1/";

    #[tokio::test]
    #[ignore]
    async fn test_wallet_rate_limit() {
        let rl = RateLimiter::new(TEST_URL).await.unwrap();
        let wallet = Address::ZERO;
        let key = format!("rate:wallet:{wallet}");
        rl.reset(&key).await.unwrap();
        for _ in 0..WALLET_LIMIT {
            assert_eq!(rl.check_wallet(&wallet).await.unwrap(), RateLimitResult::Allowed);
        }
        assert_eq!(
            rl.check_wallet(&wallet).await.unwrap(),
            RateLimitResult::Denied {
                limit: WALLET_LIMIT,
                window_secs: WALLET_WINDOW_SECS,
            }
        );
        rl.reset(&key).await.unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn test_ip_rate_limit() {
        let rl = RateLimiter::new(TEST_URL).await.unwrap();
        let ip = "203.0.113.7";
        let key = format!("rate:ip:{ip}");
        rl.reset(&key).await.unwrap();
        for _ in 0..IP_LIMIT {
            assert_eq!(rl.check_ip(ip).await.unwrap(), RateLimitResult::Allowed);
        }
        assert_eq!(
            rl.check_ip(ip).await.unwrap(),
            RateLimitResult::Denied {
                limit: IP_LIMIT,
                window_secs: IP_WINDOW_SECS,
            }
        );
        rl.reset(&key).await.unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn test_node_rate_limit() {
        let rl = RateLimiter::new(TEST_URL).await.unwrap();
        let node_id = NodeId("test-node".to_string());
        let key = format!("rate:node:{}", node_id.0);
        rl.reset(&key).await.unwrap();
        for _ in 0..NODE_LIMIT {
            assert_eq!(rl.check_node(&node_id).await.unwrap(), RateLimitResult::Allowed);
        }
        assert_eq!(
            rl.check_node(&node_id).await.unwrap(),
            RateLimitResult::Denied {
                limit: NODE_LIMIT,
                window_secs: NODE_WINDOW_SECS,
            }
        );
        rl.reset(&key).await.unwrap();
    }
}
