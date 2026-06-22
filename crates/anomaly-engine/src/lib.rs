#![deny(warnings)]
#![deny(missing_docs)]
//! Five-trigger anomaly scoring and AnomalyEvent publishing.

/// Builds an anomaly event from the triggers fired during a session.
pub fn build_anomaly_event(
    _session_id: common::SessionId,
    _triggers: Vec<common::InvalidReason>,
) -> common::AnomalyEvent {
    todo!()
}
