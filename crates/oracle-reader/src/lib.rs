#![deny(warnings)]
#![deny(missing_docs)]
//! Oracle price reader: Chainlink (XAU, XAG) on-chain feeds and Pyth Hermes
//! (XPT, WTI) HTTP feeds, normalized to 8-decimal scaled integers and sampled
//! into the session TWAP calculator.

use std::sync::Arc;

use alloy::primitives::{address, Address, I256, U256};
use alloy::providers::{Provider, ProviderBuilder, RootProvider};
use alloy::transports::TransportError;
use chrono::Utc;
use common::Commodity;
use serde::Deserialize;
use twap_calculator::{OracleSource, PriceSample, TwapCalculator};

/// Verified Ethereum mainnet Chainlink XAU/USD (gold) proxy address.
pub const CHAINLINK_XAU_USD: &str = "0x214eD9Da11D2fbe465a6fc601a91E62EbEc1a0D6";
/// Verified Ethereum mainnet Chainlink XAG/USD (silver) proxy address.
pub const CHAINLINK_XAG_USD: &str = "0x379589227b15F1a12195D3f2d90bBc9F31f95235";
/// Pyth XPT/USD (platinum) price feed id.
pub const PYTH_XPT_USD_FEED_ID: &str =
    "398e4bbc7cbf89d6648c21e08019d878967677753b3096799595c78f805a34e5";
/// Pyth WTI/USD (crude oil) price feed id. This is the front-month dated
/// contract (WTIQ6, 21 July 2026); Pyth exposes no perpetual WTI feed, so this
/// id must be rolled forward as contracts expire.
pub const PYTH_WTI_USD_FEED_ID: &str =
    "05e7c9b556df67e455c52ea2d31658744e3f4ade60db7dab887008844f2ae472";
/// Base URL of the Pyth Hermes price service.
pub const PYTH_HERMES_URL: &str = "https://hermes.pyth.network";

const TARGET_DECIMALS: i32 = 8;

mod chainlink {
    #![allow(missing_docs, clippy::all, clippy::pedantic)]
    alloy::sol! {
        #[sol(rpc)]
        interface AggregatorV3Interface {
            function latestRoundData() external view returns (
                uint80 roundId,
                int256 answer,
                uint256 startedAt,
                uint256 updatedAt,
                uint80 answeredInRound
            );
        }
    }
}

/// Configuration for a single Chainlink price feed.
#[derive(Debug, Clone)]
pub struct ChainlinkFeed {
    /// Commodity this feed prices.
    pub commodity: Commodity,
    /// On-chain aggregator proxy address.
    pub address: Address,
    /// Decimal precision the feed reports answers in.
    pub decimals: u8,
}

/// Configuration for a single Pyth Hermes price feed.
#[derive(Debug, Clone)]
pub struct PythFeed {
    /// Commodity this feed prices.
    pub commodity: Commodity,
    /// Pyth price feed id (64-char hex, no `0x` prefix).
    pub feed_id: String,
}

/// Returns the default Chainlink feeds available on Ethereum mainnet.
pub fn default_chainlink_feeds() -> Vec<ChainlinkFeed> {
    vec![
        ChainlinkFeed {
            commodity: Commodity::Gold,
            address: address!("0x214ed9da11d2fbe465a6fc601a91e62ebec1a0d6"),
            decimals: 8,
        },
        ChainlinkFeed {
            commodity: Commodity::Silver,
            address: address!("0x379589227b15f1a12195d3f2d90bbc9f31f95235"),
            decimals: 8,
        },
    ]
}

/// Returns the default Pyth feeds for commodities without a mainnet Chainlink
/// feed.
pub fn default_pyth_feeds() -> Vec<PythFeed> {
    vec![
        PythFeed {
            commodity: Commodity::Platinum,
            feed_id: PYTH_XPT_USD_FEED_ID.to_string(),
        },
        PythFeed {
            commodity: Commodity::CrudeOil,
            feed_id: PYTH_WTI_USD_FEED_ID.to_string(),
        },
    ]
}

/// Errors returned by the oracle reader.
#[derive(Debug)]
pub enum OracleReaderError {
    /// RPC transport error.
    Provider(TransportError),
    /// HTTP error talking to the Pyth Hermes service.
    Http(reqwest::Error),
    /// No configured feed exists for the requested commodity.
    FeedNotFound(Commodity),
    /// The feed reported a non-positive or unrepresentable price.
    InvalidPrice,
    /// A response could not be parsed.
    Parse(String),
}

impl std::fmt::Display for OracleReaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Provider(e) => write!(f, "provider error: {e}"),
            Self::Http(e) => write!(f, "http error: {e}"),
            Self::FeedNotFound(commodity) => write!(f, "no feed configured for {commodity:?}"),
            Self::InvalidPrice => write!(f, "feed returned an invalid price"),
            Self::Parse(msg) => write!(f, "parse error: {msg}"),
        }
    }
}

impl std::error::Error for OracleReaderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Provider(e) => Some(e),
            Self::Http(e) => Some(e),
            Self::FeedNotFound(_) | Self::InvalidPrice | Self::Parse(_) => None,
        }
    }
}

impl From<TransportError> for OracleReaderError {
    fn from(e: TransportError) -> Self {
        Self::Provider(e)
    }
}

impl From<reqwest::Error> for OracleReaderError {
    fn from(e: reqwest::Error) -> Self {
        Self::Http(e)
    }
}

#[derive(Deserialize)]
struct HermesResponse {
    parsed: Vec<HermesParsed>,
}

#[derive(Deserialize)]
struct HermesParsed {
    price: HermesPrice,
}

#[derive(Deserialize)]
struct HermesPrice {
    price: String,
    expo: i32,
}

/// Converts a raw Pyth price and exponent to an 8-decimal scaled integer.
///
/// Returns `None` for non-positive prices or when the rescaled value overflows
/// `u128`.
pub fn convert_pyth_price(price: i64, expo: i32) -> Option<u128> {
    if price <= 0 {
        return None;
    }
    let base = price as u128;
    let shift = TARGET_DECIMALS + expo;
    if shift >= 0 {
        let factor = 10u128.checked_pow(u32::try_from(shift).ok()?)?;
        base.checked_mul(factor)
    } else {
        let factor = 10u128.checked_pow(u32::try_from(-shift).ok()?)?;
        Some(base / factor)
    }
}

async fn fetch_chainlink(
    provider: &RootProvider,
    feed: &ChainlinkFeed,
) -> Result<u128, OracleReaderError> {
    let contract = chainlink::AggregatorV3Interface::new(feed.address, provider.clone());
    let round = contract
        .latestRoundData()
        .call()
        .await
        .map_err(|e| OracleReaderError::Parse(format!("chainlink call failed: {e}")))?;
    if round.answer <= I256::ZERO {
        return Err(OracleReaderError::InvalidPrice);
    }
    let raw: U256 = round.answer.into_raw();
    u128::try_from(raw).map_err(|_| OracleReaderError::InvalidPrice)
}

async fn fetch_pyth(
    client: &reqwest::Client,
    base_url: &str,
    feed: &PythFeed,
) -> Result<u128, OracleReaderError> {
    let url = format!("{base_url}/v2/updates/price/latest?ids[]={}", feed.feed_id);
    let response = client.get(&url).send().await?;
    let body: HermesResponse = response.json().await?;
    let entry = body
        .parsed
        .first()
        .ok_or_else(|| OracleReaderError::Parse("empty pyth response".to_string()))?;
    let raw_price: i64 = entry
        .price
        .price
        .parse()
        .map_err(|_| OracleReaderError::Parse("invalid pyth price value".to_string()))?;
    convert_pyth_price(raw_price, entry.price.expo).ok_or(OracleReaderError::InvalidPrice)
}

/// Reads commodity prices from Chainlink and Pyth, normalized to 8 decimals.
pub struct OracleReader {
    provider: Arc<RootProvider>,
    http_client: reqwest::Client,
    chainlink_feeds: Vec<ChainlinkFeed>,
    pyth_feeds: Vec<PythFeed>,
    pyth_hermes_url: String,
}

impl OracleReader {
    /// Builds a reader over the JSON-RPC endpoint at `rpc_url` with the given
    /// Chainlink and Pyth feed configurations.
    pub async fn new(
        rpc_url: &str,
        chainlink_feeds: Vec<ChainlinkFeed>,
        pyth_feeds: Vec<PythFeed>,
    ) -> Result<Self, OracleReaderError> {
        let url = rpc_url
            .parse()
            .map_err(|_| OracleReaderError::Parse("invalid rpc url".to_string()))?;
        let provider = ProviderBuilder::new().connect_http(url);
        let root = provider.root().clone();
        Ok(Self {
            provider: Arc::new(root),
            http_client: reqwest::Client::new(),
            chainlink_feeds,
            pyth_feeds,
            pyth_hermes_url: PYTH_HERMES_URL.to_string(),
        })
    }

    /// Reads the latest price from a Chainlink feed as an 8-decimal integer.
    pub async fn read_chainlink_price(
        &self,
        feed: &ChainlinkFeed,
    ) -> Result<u128, OracleReaderError> {
        fetch_chainlink(self.provider.as_ref(), feed).await
    }

    /// Reads the latest price from a Pyth feed, normalized to 8 decimals.
    pub async fn read_pyth_price(&self, feed: &PythFeed) -> Result<u128, OracleReaderError> {
        fetch_pyth(&self.http_client, &self.pyth_hermes_url, feed).await
    }

    /// Reads the price for `commodity`, preferring a Chainlink feed and falling
    /// back to Pyth.
    pub async fn read_price(&self, commodity: Commodity) -> Result<u128, OracleReaderError> {
        if let Some(feed) = self
            .chainlink_feeds
            .iter()
            .find(|feed| feed.commodity == commodity)
        {
            return self.read_chainlink_price(feed).await;
        }
        if let Some(feed) = self.pyth_feeds.iter().find(|feed| feed.commodity == commodity) {
            return self.read_pyth_price(feed).await;
        }
        Err(OracleReaderError::FeedNotFound(commodity))
    }

    /// Reads every configured Chainlink and Pyth feed concurrently.
    pub async fn read_all_prices(&self) -> Vec<(Commodity, Result<u128, OracleReaderError>)> {
        let mut handles = Vec::with_capacity(self.chainlink_feeds.len() + self.pyth_feeds.len());
        for feed in &self.chainlink_feeds {
            let provider = Arc::clone(&self.provider);
            let feed = feed.clone();
            let commodity = feed.commodity;
            handles.push(tokio::spawn(async move {
                (commodity, fetch_chainlink(provider.as_ref(), &feed).await)
            }));
        }
        for feed in &self.pyth_feeds {
            let client = self.http_client.clone();
            let base_url = self.pyth_hermes_url.clone();
            let feed = feed.clone();
            let commodity = feed.commodity;
            handles.push(tokio::spawn(async move {
                (commodity, fetch_pyth(&client, &base_url, &feed).await)
            }));
        }

        let mut results = Vec::with_capacity(handles.len());
        for handle in handles {
            match handle.await {
                Ok(pair) => results.push(pair),
                Err(e) => tracing::error!(error = %e, "oracle read task failed to join"),
            }
        }
        results
    }

    /// Reads the current price for `commodity` and appends it to `calculator`.
    pub async fn sample_into_twap(
        &self,
        commodity: Commodity,
        calculator: &mut TwapCalculator,
    ) -> Result<(), OracleReaderError> {
        let price = self.read_price(commodity).await?;
        let oracle = if self
            .chainlink_feeds
            .iter()
            .any(|feed| feed.commodity == commodity)
        {
            OracleSource::Chainlink
        } else {
            OracleSource::Pyth
        };
        calculator.add_sample(PriceSample {
            price,
            sampled_at: Utc::now(),
            oracle,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn test_default_chainlink_feeds() {
        let feeds = default_chainlink_feeds();
        assert_eq!(feeds.len(), 2);
        assert!(Address::from_str(CHAINLINK_XAU_USD).is_ok());
        assert!(Address::from_str(CHAINLINK_XAG_USD).is_ok());
    }

    #[test]
    fn test_default_pyth_feeds() {
        assert_eq!(default_pyth_feeds().len(), 2);
    }

    #[test]
    fn test_pyth_price_conversion() {
        assert_eq!(convert_pyth_price(320_400, -2), Some(320_400_000_000));
        assert_eq!(convert_pyth_price(0, -2), None);
        assert_eq!(convert_pyth_price(-5, -2), None);
    }

    #[tokio::test]
    #[ignore]
    async fn test_read_xau_price_live() {
        let rpc = std::env::var("RPC_URL").unwrap();
        let reader = OracleReader::new(&rpc, default_chainlink_feeds(), default_pyth_feeds())
            .await
            .unwrap();
        let price = reader.read_price(Commodity::Gold).await.unwrap();
        assert!(price > 100_000_000_000);
    }

    #[tokio::test]
    #[ignore]
    async fn test_read_xpt_price_live() {
        let rpc = std::env::var("RPC_URL").unwrap();
        let reader = OracleReader::new(&rpc, default_chainlink_feeds(), default_pyth_feeds())
            .await
            .unwrap();
        let price = reader.read_price(Commodity::Platinum).await.unwrap();
        assert!(price > 0);
    }
}
