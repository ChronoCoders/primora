#![deny(warnings)]
#![deny(missing_docs)]
//! Off-chain session TWAP sampling, averaging, and finalization.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const MIN_VALID_SESSION_SECS: i64 = 600;

/// Oracle source a price sample was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OracleSource {
    /// Chainlink price feed.
    Chainlink,
    /// Pyth Network price feed.
    Pyth,
    /// API3 dAPI price feed.
    Api3,
}

/// A single oracle price observation taken during a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceSample {
    /// Price as a scaled integer with 8 decimals (Chainlink standard).
    pub price: u128,
    /// Time the sample was taken.
    pub sampled_at: DateTime<Utc>,
    /// Oracle the sample was read from.
    pub oracle: OracleSource,
}

/// Final time-weighted average price result for a completed session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TwapResult {
    /// Computed time-weighted average price as a scaled integer (8 decimals).
    pub twap: u128,
    /// Number of samples that contributed to the average.
    pub sample_count: usize,
    /// Session start time.
    pub session_start: DateTime<Utc>,
    /// Session end time.
    pub session_end: DateTime<Utc>,
    /// Whether the session met the minimum duration for full TWAP validity.
    pub is_valid: bool,
}

/// Accumulates oracle price samples for a session and computes the TWAP.
pub struct TwapCalculator {
    samples: Vec<PriceSample>,
    session_start: DateTime<Utc>,
}

impl TwapCalculator {
    /// Creates an empty calculator for a session starting at `session_start`.
    pub fn new(session_start: DateTime<Utc>) -> Self {
        Self {
            samples: Vec::new(),
            session_start,
        }
    }

    /// Appends a sample taken in chronological order. Samples taken before the
    /// session start are ignored.
    pub fn add_sample(&mut self, sample: PriceSample) {
        if sample.sampled_at < self.session_start {
            return;
        }
        self.samples.push(sample);
    }

    /// Returns the simple average of all sample prices, or `None` when no
    /// samples have been collected.
    pub fn calculate(&self) -> Option<u128> {
        if self.samples.is_empty() {
            return None;
        }
        let count = self.samples.len() as u128;
        let sum: u128 = self.samples.iter().map(|sample| sample.price).sum();
        Some(sum / count)
    }

    /// Returns true when the span from session start to the latest sample is at
    /// least the minimum valid session duration of 600 seconds.
    pub fn is_valid(&self) -> bool {
        match self.samples.last() {
            Some(latest) => {
                (latest.sampled_at - self.session_start).num_seconds() >= MIN_VALID_SESSION_SECS
            }
            None => false,
        }
    }

    /// Returns the number of collected samples.
    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }

    /// Builds the final [`TwapResult`] for `session_end`, or `None` when there
    /// are no samples to average.
    pub fn finalize(&self, session_end: DateTime<Utc>) -> Option<TwapResult> {
        let twap = self.calculate()?;
        Some(TwapResult {
            twap,
            sample_count: self.samples.len(),
            session_start: self.session_start,
            session_end,
            is_valid: self.is_valid(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn start() -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap()
    }

    fn sample(price: u128, at: DateTime<Utc>) -> PriceSample {
        PriceSample {
            price,
            sampled_at: at,
            oracle: OracleSource::Chainlink,
        }
    }

    #[test]
    fn test_empty_returns_none() {
        let calc = TwapCalculator::new(start());
        assert_eq!(calc.calculate(), None);
    }

    #[test]
    fn test_single_sample() {
        let mut calc = TwapCalculator::new(start());
        calc.add_sample(sample(320_400_000_000, start()));
        assert_eq!(calc.calculate(), Some(320_400_000_000));
    }

    #[test]
    fn test_average_two_samples() {
        let mut calc = TwapCalculator::new(start());
        calc.add_sample(sample(320_400_000_000, start()));
        calc.add_sample(sample(320_600_000_000, start() + Duration::seconds(1)));
        assert_eq!(calc.calculate(), Some(320_500_000_000));
    }

    #[test]
    fn test_is_valid_true() {
        let mut calc = TwapCalculator::new(start());
        calc.add_sample(sample(320_400_000_000, start() + Duration::seconds(601)));
        assert!(calc.is_valid());
    }

    #[test]
    fn test_is_valid_false_short_session() {
        let mut calc = TwapCalculator::new(start());
        calc.add_sample(sample(320_400_000_000, start() + Duration::seconds(300)));
        assert!(!calc.is_valid());
    }

    #[test]
    fn test_is_valid_false_no_samples() {
        let calc = TwapCalculator::new(start());
        assert!(!calc.is_valid());
    }

    #[test]
    fn test_ignore_sample_before_start() {
        let mut calc = TwapCalculator::new(start());
        calc.add_sample(sample(320_400_000_000, start() - Duration::seconds(1)));
        assert_eq!(calc.sample_count(), 0);
    }

    #[test]
    fn test_finalize_valid() {
        let mut calc = TwapCalculator::new(start());
        calc.add_sample(sample(320_400_000_000, start()));
        calc.add_sample(sample(320_600_000_000, start() + Duration::seconds(700)));
        let result = calc.finalize(start() + Duration::seconds(700));
        assert!(result.is_some());
        let result = result.unwrap();
        assert!(result.is_valid);
        assert_eq!(result.twap, 320_500_000_000);
        assert_eq!(result.sample_count, 2);
    }

    #[test]
    fn test_finalize_empty() {
        let calc = TwapCalculator::new(start());
        assert!(calc.finalize(start() + Duration::seconds(700)).is_none());
    }
}
