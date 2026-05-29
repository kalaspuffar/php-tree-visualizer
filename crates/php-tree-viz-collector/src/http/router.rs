//! Builds the axum `Router`. The shape splits the app into two
//! sub-routers so middleware applies only where it belongs:
//!
//! ```text
//!   Top-level router
//!     ├── GET  /health   → health::liveness     (unauthenticated)
//!     ├── GET  /ready    → health::readiness    (unauthenticated)
//!     └── merge(protected)
//!            └── POST /ingest/v1   wrapped in:
//!                  logging::log_request
//!                  auth::require_bearer_token
//!                  content_type::require_msgpack_content_type
//!                  ingest::ingest
//! ```
//!
//! The protected sub-router carries every middleware that
//! defends or instruments ingest (request logging, auth, content
//! type). Probe routes are registered on the top-level router and
//! therefore see none of them — probes return without an auth
//! check, without a content-type check, and without per-request
//! log lines. This is intentional per the `collector-health`
//! capability: probes never carry credentials, and a successful
//! probe leaves no journal trace so the standard k8s 10-second
//! probe cadence does not drown signal.
//!
//! Layer order for the protected sub-router is "outermost first":
//! `logging::log_request` is the outermost `.layer(...)` so it
//! sees the final response status (including 401s short-circuited
//! by auth and 415s short-circuited by content-type). The two
//! `route_layer` calls are inside it, applied to matched routes
//! only — a request to an unknown path returns 404 without
//! invoking either check.
//!
//! Note: an unknown path on the top-level router (e.g.
//! `GET /elsewhere`) still returns 404, exactly as before. The
//! probe routes are MATCHED paths that explicitly skip auth, not
//! a fallthrough that bypasses it.

use axum::extract::DefaultBodyLimit;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::routing::{get, post};
use axum::Router;

use super::{auth, content_type, health, ingest, logging, SharedState};

pub fn build(state: SharedState) -> Router {
    let protected = Router::new()
        // axum's `DefaultBodyLimit` defaults to 2 MiB and would
        // short-circuit oversize requests with a non-spec response
        // body before our handler runs. Disable it so the handler
        // can enforce the operator-configured cap and produce the
        // documented `{"error":"too_large"}` shape.
        .route(
            "/ingest/v1",
            post(ingest::ingest).layer(DefaultBodyLimit::disable()),
        )
        .route_layer(from_fn(content_type::require_msgpack_content_type))
        .route_layer(from_fn_with_state(
            state.clone(),
            auth::require_bearer_token,
        ))
        .layer(from_fn(logging::log_request));

    Router::new()
        .route("/health", get(health::liveness))
        .route("/ready", get(health::readiness))
        .merge(protected)
        .with_state(state)
}
