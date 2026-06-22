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
    /// the aggregate suspicion level.
    ///
    /// Triggers are deduplicated by variant; a reason appearing twice counts once.
    /// A full or closed channel is logged via `tracing::warn!` and does not error.
    pub fn process(
        &self,
        session_id: SessionId,
        wallet: Address,
        results: Vec<ValidationResult>,
    ) -> SuspicionLevel {
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
        let event = AnomalyEvent {
            session_id,
            wallet,
            score: 0,
            triggers,
            level,
            timestamp: Utc::now(),
        };
        if let Err(err) = self.sender.try_send(event) {
            tracing::warn!(error = %err, "anomaly event channel send failed");
        }
        level
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

    fn run(results: Vec<ValidationResult>) -> SuspicionLevel {
        let (tx, _rx) = channel(32);
        let engine = AnomalyEngine::new(tx);
        engine.process(SessionId("s".into()), Address::ZERO, results)
    }

    #[test]
    fn test_no_triggers() {
        let level = run(vec![]);
        assert_eq!(level, SuspicionLevel::Low);
        assert!(!should_slash_vote(&level));
    }

    #[test]
    fn test_one_invalid() {
        let level = run(vec![ValidationResult::Invalid(InvalidReason::TimingAnomaly)]);
        assert_eq!(level, SuspicionLevel::Medium);
        assert!(!should_slash_vote(&level));
    }

    #[test]
    fn test_two_invalids() {
        let level = run(vec![
            ValidationResult::Invalid(InvalidReason::TimingAnomaly),
            ValidationResult::Invalid(InvalidReason::HashrateImpossible),
        ]);
        assert_eq!(level, SuspicionLevel::High);
        assert!(should_slash_vote(&level));
    }

    #[test]
    fn test_deduplication() {
        let level = run(vec![
            ValidationResult::Invalid(InvalidReason::TimingAnomaly),
            ValidationResult::Invalid(InvalidReason::TimingAnomaly),
        ]);
        assert_eq!(level, SuspicionLevel::Medium);
    }

    #[test]
    fn test_suspicious_high_counts() {
        let level = run(vec![
            ValidationResult::Suspicious(SuspicionLevel::High),
            ValidationResult::Invalid(InvalidReason::TimingAnomaly),
        ]);
        assert_eq!(level, SuspicionLevel::High);
    }

    #[test]
    fn test_suspicious_low_ignored() {
        let level = run(vec![ValidationResult::Suspicious(SuspicionLevel::Low)]);
        assert_eq!(level, SuspicionLevel::Low);
    }

    #[test]
    fn test_high_suspicion_alone() {
        let level = run(vec![ValidationResult::Suspicious(SuspicionLevel::High)]);
        assert_eq!(level, SuspicionLevel::Medium);
    }
}
