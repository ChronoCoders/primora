use alloy_primitives::Address;
use anomaly_engine::AnomalyEngine;
use common::{InvalidReason, SessionId, SuspicionLevel, ValidationResult};
use tokio::sync::mpsc::channel;

#[tokio::test]
async fn test_no_triggers_low() {
    let (tx, mut rx) = channel(16);
    let engine = AnomalyEngine::new(tx);
    let level = engine.process(
        SessionId("sess-clean".to_string()),
        Address::ZERO,
        vec![ValidationResult::Valid],
    );
    assert_eq!(level, SuspicionLevel::Low);

    let Some(event) = rx.recv().await else {
        panic!("no anomaly event published");
    };
    assert_eq!(event.level, SuspicionLevel::Low);
    assert!(event.triggers.is_empty());
}

#[tokio::test]
async fn test_single_trigger_medium() {
    let (tx, mut rx) = channel(16);
    let engine = AnomalyEngine::new(tx);
    let level = engine.process(
        SessionId("sess-single".to_string()),
        Address::ZERO,
        vec![ValidationResult::Invalid(InvalidReason::TimingAnomaly)],
    );
    assert_eq!(level, SuspicionLevel::Medium);

    let Some(event) = rx.recv().await else {
        panic!("no anomaly event published");
    };
    assert_eq!(event.level, SuspicionLevel::Medium);
    assert_eq!(event.session_id, SessionId("sess-single".to_string()));
    assert_eq!(event.triggers, vec![InvalidReason::TimingAnomaly]);
}

#[tokio::test]
async fn test_multiple_triggers_high() {
    let (tx, mut rx) = channel(16);
    let engine = AnomalyEngine::new(tx);
    let level = engine.process(
        SessionId("sess-multi".to_string()),
        Address::ZERO,
        vec![
            ValidationResult::Invalid(InvalidReason::TimingAnomaly),
            ValidationResult::Invalid(InvalidReason::HashrateImpossible),
        ],
    );
    assert_eq!(level, SuspicionLevel::High);

    let Some(event) = rx.recv().await else {
        panic!("no anomaly event published");
    };
    assert_eq!(event.level, SuspicionLevel::High);
    assert_eq!(event.triggers.len(), 2);
}

#[tokio::test]
async fn test_duplicate_triggers_deduplicated() {
    let (tx, mut rx) = channel(16);
    let engine = AnomalyEngine::new(tx);
    let level = engine.process(
        SessionId("sess-dup".to_string()),
        Address::ZERO,
        vec![
            ValidationResult::Invalid(InvalidReason::TimingAnomaly),
            ValidationResult::Invalid(InvalidReason::TimingAnomaly),
        ],
    );
    assert_eq!(level, SuspicionLevel::Medium);

    let Some(event) = rx.recv().await else {
        panic!("no anomaly event published");
    };
    assert_eq!(event.triggers, vec![InvalidReason::TimingAnomaly]);
}
