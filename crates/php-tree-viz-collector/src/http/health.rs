//! HTTP probe endpoints.
//!
//! Implements the `collector-health` capability: `GET /health`
//! (liveness, unconditionally 200) and `GET /ready` (readiness,
//! 200 when the bounded ingest mpsc has spare capacity, 503 when
//! near saturation). Both routes are unauthenticated by design —
//! they carry no operationally-sensitive data and probes never
//! carry credentials (kubelet, ELB, systemd, monit all omit them).
//!
//! Liveness emits no log line. Readiness is silent on the 200 path
//! and emits exactly one `warn`-level `readiness degraded` event
//! on the 503 path. The router (`router::build`) registers these
//! routes on the top-level router and the per-request
//! `logging::log_request` layer lives inside the protected
//! `/ingest/*` sub-router, so probe traffic does not flood the
//! journal.

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use super::SharedState;

/// Spare-slot ratio below which readiness reports `degraded`. A
/// queue at or near saturation predicts the imminent
/// `503 queue_full` ingest failure mode (INV-7); the LB upstream
/// uses readiness to drain the collector before genuine 503s
/// start. 10% of `queue_capacity` (default 256 ⇒ ~26 slots) gives
/// the LB roughly hundreds of ms to react.
const READINESS_SPARE_RATIO: f64 = 0.10;

/// Liveness probe. Returns 200 with `{"status":"ok"}` unconditionally.
/// The handler touches no shared state — a 200 here proves the
/// runtime is alive, the listener accepted the connection, and
/// axum routed the request.
pub async fn liveness(State(_state): State<SharedState>) -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"status":"ok"}"#,
    )
}

/// Readiness probe. Inspects the bounded ingest mpsc's spare-slot
/// ratio: 200 when at least `READINESS_SPARE_RATIO` of slots are
/// free, 503 otherwise. Lock-free, O(1), decoder-independent —
/// reading mpsc capacity does not contend with batch decoding.
pub async fn readiness(State(state): State<SharedState>) -> Response {
    let capacity = state.batch_tx.capacity();
    let max_capacity = state.batch_tx.max_capacity();
    // `max_capacity` is the configured cap; it never returns zero
    // for a sender constructed via `mpsc::channel(N)` with `N > 0`.
    // The collector always uses `queue_capacity >= 1` per the
    // bounded-mpsc capability, so this division is safe.
    let spare_ratio = capacity as f64 / max_capacity as f64;

    if spare_ratio >= READINESS_SPARE_RATIO {
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"status":"ready"}"#,
        )
            .into_response();
    }

    let queue_capacity_used = 1.0 - spare_ratio;
    // One log line per probe-tick when degraded is the right
    // cadence — quiet on healthy steady state, noisy when the
    // collector is genuinely falling behind.
    tracing::warn!(
        queue_capacity_used,
        queue_capacity_max = max_capacity as u64,
        "readiness degraded"
    );
    let body = format!(
        r#"{{"status":"degraded","reason":"queue_near_full","queue_capacity_used":{queue_capacity_used:.2}}}"#
    );
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    //! Unit tests against the handler functions. The integration
    //! tests in `tests/http_skeleton.rs` exercise the wire-level
    //! contract; these tests pin the readiness predicate and the
    //! response shapes deterministically, without timing-sensitive
    //! mpsc saturation through the listener.
    use super::*;
    use crate::config::SecretString;
    use crate::http::{AppState, BatchSubmission, SharedState};
    use axum::body::to_bytes;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, RwLock};
    use tokio::sync::mpsc;
    use tracing_subscriber::fmt::MakeWriter;
    use tracing_subscriber::EnvFilter;

    /// Build a `SharedState` for unit-testing the probe handlers.
    /// Returns the receiver alongside the state so the test keeps
    /// the channel open for the duration of the test — dropping it
    /// would close the channel and `try_reserve` would error with
    /// `Closed`. Tests that don't care about the receiver should
    /// bind it as `_rx` and let it live to the end of the function.
    fn shared_state_with_capacity(
        capacity: usize,
    ) -> (
        SharedState,
        mpsc::Sender<BatchSubmission>,
        mpsc::Receiver<BatchSubmission>,
    ) {
        let (tx, rx) = mpsc::channel::<BatchSubmission>(capacity);
        let state = Arc::new(AppState {
            expected_token: SecretString::from("unit-test-token"),
            max_body_bytes: 1024,
            tmp_dir: PathBuf::from("/tmp/unit-test-not-used"),
            traces_dir: PathBuf::from("/tmp/unit-test-not-used"),
            trace_locks: RwLock::new(HashMap::new()),
            batch_tx: tx.clone(),
        });
        (state, tx, rx)
    }

    async fn read_body_to_string(response: Response) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn liveness_returns_200_with_status_ok_body() {
        let (state, _tx, _rx) = shared_state_with_capacity(8);
        let response = liveness(State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "application/json"
        );
        let body = read_body_to_string(response).await;
        assert_eq!(body, r#"{"status":"ok"}"#);
    }

    #[tokio::test]
    async fn readiness_returns_200_when_queue_is_empty() {
        let (state, _tx, _rx) = shared_state_with_capacity(8);
        let response = readiness(State(state)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = read_body_to_string(response).await;
        assert_eq!(body, r#"{"status":"ready"}"#);
    }

    #[tokio::test]
    async fn readiness_returns_200_when_queue_has_just_above_threshold_spare() {
        // Capacity 10, reserve 8 → 2 spare → ratio 0.20 → above 0.10.
        let (state, tx, _rx) = shared_state_with_capacity(10);
        let mut reservations = Vec::new();
        for _ in 0..8 {
            reservations.push(tx.try_reserve().unwrap());
        }
        let response = readiness(State(state)).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn readiness_returns_503_when_spare_ratio_below_threshold() {
        // Capacity 10, reserve 9 → 1 spare → ratio 0.10 ≮ 0.10 …
        // careful: the predicate is `spare_ratio >= 0.10`, so we
        // need ratio STRICTLY below 0.10. Reserve 10 (all slots) →
        // ratio 0.0 → 503.
        let (state, tx, _rx) = shared_state_with_capacity(10);
        let mut reservations = Vec::new();
        for _ in 0..10 {
            reservations.push(tx.try_reserve().unwrap());
        }
        let response = readiness(State(state)).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = read_body_to_string(response).await;
        assert!(
            body.contains(r#""status":"degraded""#),
            "body missing degraded marker: {body}"
        );
        assert!(
            body.contains(r#""reason":"queue_near_full""#),
            "body missing reason marker: {body}"
        );
        assert!(
            body.contains(r#""queue_capacity_used":1.00"#),
            "expected queue_capacity_used:1.00 in body: {body}"
        );
    }

    /// Captures subscriber output into a shared `Vec<u8>` so the
    /// test can assert on the emitted log lines for the degraded
    /// readiness path.
    #[derive(Clone, Default)]
    struct CapturingWriter(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CapturingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for CapturingWriter {
        type Writer = CapturingWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[tokio::test]
    async fn readiness_degraded_emits_one_warn_event() {
        let capture = CapturingWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_env_filter(EnvFilter::new("warn"))
            .with_ansi(false)
            .finish();

        let (state, tx, _rx) = shared_state_with_capacity(4);
        let mut reservations = Vec::new();
        for _ in 0..4 {
            reservations.push(tx.try_reserve().unwrap());
        }

        // `set_default` scopes the subscriber to this thread until
        // the guard is dropped. Lets us `.await` an async fn while
        // pinning the event capture, without nesting block_on
        // inside an already-running tokio runtime.
        let _guard = tracing::subscriber::set_default(subscriber);
        let response = readiness(State(state.clone())).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        drop(_guard);

        let captured = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(
            captured.contains("readiness degraded"),
            "expected 'readiness degraded' in log: {captured:?}"
        );
        assert!(
            captured.contains("queue_capacity_used="),
            "expected queue_capacity_used field: {captured:?}"
        );
        assert!(
            captured.contains("queue_capacity_max=4"),
            "expected queue_capacity_max=4 field: {captured:?}"
        );
    }

    #[tokio::test]
    async fn readiness_healthy_emits_no_log_event() {
        let capture = CapturingWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_env_filter(EnvFilter::new("trace"))
            .with_ansi(false)
            .finish();

        let (state, _tx, _rx) = shared_state_with_capacity(8);

        let _guard = tracing::subscriber::set_default(subscriber);
        let response = readiness(State(state.clone())).await;
        assert_eq!(response.status(), StatusCode::OK);
        drop(_guard);

        let captured = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(
            !captured.contains("readiness"),
            "200 path must emit no log line: {captured:?}"
        );
    }

    #[tokio::test]
    async fn liveness_emits_no_log_event() {
        let capture = CapturingWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_env_filter(EnvFilter::new("trace"))
            .with_ansi(false)
            .finish();

        let (state, _tx, _rx) = shared_state_with_capacity(8);

        let _guard = tracing::subscriber::set_default(subscriber);
        let response = liveness(State(state.clone())).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        drop(_guard);

        let captured = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(
            captured.is_empty() || !captured.contains("liveness"),
            "liveness must emit no log line: {captured:?}"
        );
    }
}
