//! Builds the axum `Router`. The shape is deliberately stable —
//! subsequent changes add layers and handlers around this skeleton
//! rather than rewriting it.

use axum::middleware::{from_fn, from_fn_with_state};
use axum::routing::post;
use axum::Router;

use super::{auth, content_type, ingest, logging, SharedState};

/// Build the router. The layer order matters: layers are applied
/// from the outside in, so `route_layer` (or `.layer()`) calls listed
/// here go *around* the inner handler. The effective traversal for an
/// incoming request is:
///
/// ```text
///   request
///     │
///     ▼
///   log_request      (sees the final status; needed for 4xx visibility)
///     │
///     ▼
///   require_bearer_token   → 401 short-circuit
///     │
///     ▼
///   require_msgpack_content_type   → 415 short-circuit
///     │
///     ▼
///   ingest::ingest   → 501 placeholder
/// ```
///
/// Note: `route_layer` only applies to matched routes, so a request
/// to `/elsewhere` returns 404 *without* the auth check firing — the
/// 404 path skips the middleware entirely, which is the documented
/// behavior.
pub fn build(state: SharedState) -> Router {
    Router::new()
        .route("/ingest/v1", post(ingest::ingest))
        .route_layer(from_fn(content_type::require_msgpack_content_type))
        .route_layer(from_fn_with_state(
            state.clone(),
            auth::require_bearer_token,
        ))
        .layer(from_fn(logging::log_request))
        .with_state(state)
}
