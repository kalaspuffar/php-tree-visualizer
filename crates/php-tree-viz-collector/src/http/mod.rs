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
mod tmp;

use std::path::PathBuf;
use std::sync::Arc;

pub use error::HttpError;
pub use server::run;

use crate::config::SecretString;

/// State shared with axum extractors. Today carries the expected
/// bearer token, the configured body-size cap, and the resolved
/// `<data_dir>/tmp` path. Subsequent changes will add the mpsc
/// sender and the per-trace raw directory resolver.
pub struct AppState {
    pub expected_token: SecretString,
    pub max_body_bytes: u64,
    pub tmp_dir: PathBuf,
}

pub type SharedState = Arc<AppState>;

/// Response extension that carries the number of body bytes the
/// ingest handler read from the wire (including the bytes read
/// before an oversize abort). The request-logging middleware reads
/// this off the response so it can include `body_bytes=<N>` in the
/// per-request log line without re-counting.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BodyBytes(pub u64);
