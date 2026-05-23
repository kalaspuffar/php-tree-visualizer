//! HTTP layer for the collector.
//!
//! Implements the `http::server` sub-module of `SPECIFICATION.md`
//! §3.1 — bind, route, parse headers — but deliberately stops short
//! of body handling. The success path (`POST /ingest/v1` with a
//! valid `Authorization` and `Content-Type`) returns `501 Not
//! Implemented` until the next change wires up body streaming,
//! schema-version peek, tmp-file write, fsync, and atomic rename.

mod auth;
mod content_type;
mod error;
mod ingest;
mod logging;
mod router;
mod server;

use std::sync::Arc;

pub use error::HttpError;
pub use server::run;

use crate::config::SecretString;

/// State shared with axum extractors. Today carries only the expected
/// bearer token; subsequent changes will add the mpsc sender, the
/// tmp directory path, and similar request-handling resources.
pub struct AppState {
    pub expected_token: SecretString,
}

pub type SharedState = Arc<AppState>;
