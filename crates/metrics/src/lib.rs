#![deny(warnings)]
#![deny(missing_docs)]
//! Prometheus metrics registry and collectors for the Primora backend.

use once_cell::sync::Lazy;
use prometheus::{
    HistogramOpts, HistogramVec, IntCounterVec, IntGauge, Opts, TextEncoder,
};

fn counter_vec(name: &str, help: &str, labels: &[&str]) -> IntCounterVec {
    match IntCounterVec::new(Opts::new(name, help), labels) {
        Ok(metric) => metric,
        Err(error) => panic!("failed to construct counter {name}: {error}"),
    }
}

fn histogram_vec(name: &str, help: &str, labels: &[&str], buckets: Vec<f64>) -> HistogramVec {
    match HistogramVec::new(HistogramOpts::new(name, help).buckets(buckets), labels) {
        Ok(metric) => metric,
        Err(error) => panic!("failed to construct histogram {name}: {error}"),
    }
}

fn int_gauge(name: &str, help: &str) -> IntGauge {
    match IntGauge::new(name, help) {
        Ok(metric) => metric,
        Err(error) => panic!("failed to construct gauge {name}: {error}"),
    }
}

/// Total partial proof submissions by client type and result.
pub static PROOF_SUBMISSIONS_TOTAL: Lazy<IntCounterVec> = Lazy::new(|| {
    counter_vec(
        "proof_submissions_total",
        "Total partial proof submissions by client type and result",
        &["client_type", "result"],
    )
});

/// Total anomaly events by suspicion level and trigger.
pub static ANOMALY_EVENTS_TOTAL: Lazy<IntCounterVec> = Lazy::new(|| {
    counter_vec(
        "anomaly_events_total",
        "Total anomaly events by suspicion level and trigger",
        &["suspicion_level", "trigger"],
    )
});

/// Attestation duration from session end to completion in seconds.
pub static ATTESTATION_DURATION_SECONDS: Lazy<HistogramVec> = Lazy::new(|| {
    histogram_vec(
        "attestation_duration_seconds",
        "Attestation duration from session end to completion in seconds",
        &["status"],
        vec![0.1, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0],
    )
});

/// Total mint proposals by status.
pub static MINT_PROPOSALS_TOTAL: Lazy<IntCounterVec> = Lazy::new(|| {
    counter_vec(
        "mint_proposals_total",
        "Total mint proposals by status",
        &["status"],
    )
});

/// Node attestation response time in seconds.
pub static NODE_RESPONSE_TIME_SECONDS: Lazy<HistogramVec> = Lazy::new(|| {
    histogram_vec(
        "node_response_time_seconds",
        "Node attestation response time in seconds",
        &["node_id"],
        vec![0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 15.0],
    )
});

/// Current number of active mining sessions.
pub static SESSION_ACTIVE_COUNT: Lazy<IntGauge> = Lazy::new(|| {
    int_gauge(
        "session_active_count",
        "Current number of active mining sessions",
    )
});

/// Total rate limit hits by type (wallet, ip, node).
pub static RATE_LIMIT_HITS_TOTAL: Lazy<IntCounterVec> = Lazy::new(|| {
    counter_vec(
        "rate_limit_hits_total",
        "Total rate limit hits by type (wallet, ip, node)",
        &["limit_type"],
    )
});

/// Current number of sessions in the manual review queue.
pub static MANUAL_REVIEW_QUEUE_DEPTH: Lazy<IntGauge> = Lazy::new(|| {
    int_gauge(
        "manual_review_queue_depth",
        "Current number of sessions in manual review queue",
    )
});

/// Current per-block PRM mint ceiling value.
pub static MINT_CEILING_CURRENT: Lazy<IntGauge> = Lazy::new(|| {
    int_gauge(
        "mint_ceiling_current",
        "Current per-block PRM mint ceiling value",
    )
});

/// Registers all metrics with the default registry.
///
/// Intended to be called exactly once at service startup.
pub fn register_all() -> Result<(), prometheus::Error> {
    prometheus::register(Box::new(PROOF_SUBMISSIONS_TOTAL.clone()))?;
    prometheus::register(Box::new(ANOMALY_EVENTS_TOTAL.clone()))?;
    prometheus::register(Box::new(ATTESTATION_DURATION_SECONDS.clone()))?;
    prometheus::register(Box::new(MINT_PROPOSALS_TOTAL.clone()))?;
    prometheus::register(Box::new(NODE_RESPONSE_TIME_SECONDS.clone()))?;
    prometheus::register(Box::new(SESSION_ACTIVE_COUNT.clone()))?;
    prometheus::register(Box::new(RATE_LIMIT_HITS_TOTAL.clone()))?;
    prometheus::register(Box::new(MANUAL_REVIEW_QUEUE_DEPTH.clone()))?;
    prometheus::register(Box::new(MINT_CEILING_CURRENT.clone()))?;
    Ok(())
}

/// Returns the metric families currently held by the default registry.
pub fn gather_metrics() -> Vec<prometheus::proto::MetricFamily> {
    prometheus::gather()
}

/// Encodes the gathered metrics into the Prometheus text exposition format.
///
/// On encoding failure the error is logged and an empty string is returned.
pub fn metrics_handler() -> String {
    let encoder = TextEncoder::new();
    let metric_families = gather_metrics();
    let mut buffer = String::new();
    match encoder.encode_utf8(&metric_families, &mut buffer) {
        Ok(()) => buffer,
        Err(error) => {
            tracing::error!(%error, "failed to encode metrics");
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_register_all_succeeds() {
        assert!(register_all().is_ok());
    }

    #[test]
    fn test_proof_submissions_counter() {
        PROOF_SUBMISSIONS_TOTAL
            .with_label_values(&["Desktop", "Valid"])
            .inc();
        assert_eq!(
            PROOF_SUBMISSIONS_TOTAL
                .with_label_values(&["Desktop", "Valid"])
                .get(),
            1
        );
    }

    #[test]
    fn test_metrics_handler_returns_string() {
        assert!(!metrics_handler().is_empty());
    }

    #[test]
    fn test_session_gauge() {
        SESSION_ACTIVE_COUNT.inc();
        assert_eq!(SESSION_ACTIVE_COUNT.get(), 1);
        SESSION_ACTIVE_COUNT.dec();
        assert_eq!(SESSION_ACTIVE_COUNT.get(), 0);
    }
}
