#![deny(warnings)]
#![deny(missing_docs)]
//! Five-trigger anomaly scoring and AnomalyEvent publishing.

use std::mem::{discriminant, Discriminant};

use alloy_primitives::Address;
use chrono::Utc;
use common::{AnomalyEvent, InvalidReason, SessionId, SuspicionLevel, ValidationResult};
use tokio::sync::mpsc::Sender;

fn dedup_triggers(triggers: &mut Vec<InvalidReason>) {
    let mut seen: Vec<Discriminant<InvalidReason>> = Vec::new();
    triggers.retain(|reason| {
        let d = discriminant(reason);
        if seen.contains(&d) {
            false
        } else {
            seen.push(d);
            true
        }
    });
}

fn level_for(total_signals: usize) -> SuspicionLevel {
    match total_signals {
        0 => SuspicionLevel::Low,
        1 => SuspicionLevel::Medium,
        _ => SuspicionLevel::High,
    }
}

/// Anomaly score contribution per distinct anomaly signal, in basis points.
const PER_SIGNAL_BPS: u32 = 2_500;

/// Maximum anomaly score in basis points (100%).
pub const MAX_ANOMALY_SCORE_BPS: u32 = 10_000;

/// Anomaly score in basis points (0..=10000) for `total_signals` distinct
/// anomaly signals: [`PER_SIGNAL_BPS`] per signal, saturating at
/// [`MAX_ANOMALY_SCORE_BPS`]. Consistent with [`level_for`]: 0 signals scores 0
/// (clean), Medium begins at 2500 bps, High at 5000 bps.
pub fn anomaly_score_bps(total_signals: usize) -> u32 {
    u32::try_from(total_signals)
        .unwrap_or(u32::MAX)
        .saturating_mul(PER_SIGNAL_BPS)
        .min(MAX_ANOMALY_SCORE_BPS)
}

/// Outcome of scoring a session's validation results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Assessment {
    /// Aggregate suspicion level.
    pub level: SuspicionLevel,
    /// Anomaly score in basis points (0..=10000); see [`anomaly_score_bps`].
    pub score_bps: u32,
}

/// Scores session validation results and publishes anomaly events.
pub struct AnomalyEngine {
    sender: Sender<AnomalyEvent>,
}

impl AnomalyEngine {
    /// Creates an engine that publishes events to `sender`.
    pub fn new(sender: Sender<AnomalyEvent>) -> Self {
        Self { sender }
    }

    /// Scores `results` for a session, publishes an [`AnomalyEvent`], and returns
    /// the [`Assessment`] (aggregate suspicion level and basis-point score).
    ///
    /// Triggers are deduplicated by variant; a reason appearing twice counts once.
    /// A full or closed channel is logged via `tracing::warn!` and does not error.
    pub fn process(
        &self,
        session_id: SessionId,
        wallet: Address,
        results: Vec<ValidationResult>,
    ) -> Assessment {
        let mut triggers: Vec<InvalidReason> = Vec::new();
        let mut high_suspicion_count: usize = 0;
        for result in &results {
            match result {
                ValidationResult::Invalid(reason) => triggers.push(reason.clone()),
                ValidationResult::Suspicious(SuspicionLevel::High) => high_suspicion_count += 1,
                ValidationResult::Suspicious(_) | ValidationResult::Valid => {}
            }
        }
        dedup_triggers(&mut triggers);
        let total_signals = triggers.len() + high_suspicion_count;
        let level = level_for(total_signals);
        let score_bps = anomaly_score_bps(total_signals);
        let event = AnomalyEvent {
            session_id,
            wallet,
            score: score_bps,
            triggers,
            level,
            timestamp: Utc::now(),
        };
        if let Err(err) = self.sender.try_send(event) {
            tracing::warn!(error = %err, "anomaly event channel send failed");
        }
        Assessment { level, score_bps }
    }
}

/// Returns true only for [`SuspicionLevel::High`].
pub fn should_slash_vote(level: &SuspicionLevel) -> bool {
    matches!(level, SuspicionLevel::High)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::channel;

    fn run(results: Vec<ValidationResult>) -> Assessment {
        let (tx, _rx) = channel(32);
        let engine = AnomalyEngine::new(tx);
        engine.process(SessionId("s".into()), Address::ZERO, results)
    }

    #[test]
    fn test_no_triggers() {
        let a = run(vec![]);
        assert_eq!(a.level, SuspicionLevel::Low);
        assert_eq!(a.score_bps, 0);
        assert!(!should_slash_vote(&a.level));
    }

    #[test]
    fn test_one_invalid() {
        let a = run(vec![ValidationResult::Invalid(InvalidReason::TimingAnomaly)]);
        assert_eq!(a.level, SuspicionLevel::Medium);
        assert_eq!(a.score_bps, 2_500);
        assert!(!should_slash_vote(&a.level));
    }

    #[test]
    fn test_two_invalids() {
        let a = run(vec![
            ValidationResult::Invalid(InvalidReason::TimingAnomaly),
            ValidationResult::Invalid(InvalidReason::HashrateImpossible),
        ]);
        assert_eq!(a.level, SuspicionLevel::High);
        assert_eq!(a.score_bps, 5_000);
        assert!(should_slash_vote(&a.level));
    }

    #[test]
    fn test_deduplication() {
        let a = run(vec![
            ValidationResult::Invalid(InvalidReason::TimingAnomaly),
            ValidationResult::Invalid(InvalidReason::TimingAnomaly),
        ]);
        assert_eq!(a.level, SuspicionLevel::Medium);
        assert_eq!(a.score_bps, 2_500);
    }

    #[test]
    fn test_suspicious_high_counts() {
        let a = run(vec![
            ValidationResult::Suspicious(SuspicionLevel::High),
            ValidationResult::Invalid(InvalidReason::TimingAnomaly),
        ]);
        assert_eq!(a.level, SuspicionLevel::High);
        assert_eq!(a.score_bps, 5_000);
    }

    #[test]
    fn test_suspicious_low_ignored() {
        let a = run(vec![ValidationResult::Suspicious(SuspicionLevel::Low)]);
        assert_eq!(a.level, SuspicionLevel::Low);
        assert_eq!(a.score_bps, 0);
    }

    #[test]
    fn test_high_suspicion_alone() {
        let a = run(vec![ValidationResult::Suspicious(SuspicionLevel::High)]);
        assert_eq!(a.level, SuspicionLevel::Medium);
        assert_eq!(a.score_bps, 2_500);
    }

    #[test]
    fn test_score_saturates_at_max() {
        assert_eq!(anomaly_score_bps(0), 0);
        assert_eq!(anomaly_score_bps(4), MAX_ANOMALY_SCORE_BPS);
        assert_eq!(anomaly_score_bps(100), MAX_ANOMALY_SCORE_BPS);
        assert_eq!(anomaly_score_bps(usize::MAX), MAX_ANOMALY_SCORE_BPS);
    }
}
