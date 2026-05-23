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
mod storage;
mod tmp;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use tokio::sync::mpsc;
use tokio::sync::Mutex as AsyncMutex;

pub use error::HttpError;
pub use server::run;

use crate::config::SecretString;
use crate::tracekey::TraceKey;

/// One in-flight unit on the bounded ingest channel. Sent by the
/// HTTP handler after `commit_partial` returns `Ok`, consumed by
/// the receiver task in `server.rs` (a placeholder today — the
/// future decoder slice replaces it with `wire::decode + storage`).
#[derive(Debug)]
pub struct BatchSubmission {
    /// Canonical path of the renamed, fsync'd batch file. Lives
    /// under `<data_dir>/traces/<trace_key>.raw/`.
    pub path: PathBuf,
    /// Identifier for the trace the batch belongs to.
    pub trace_key: TraceKey,
}

/// State shared with axum extractors. Carries the expected bearer
/// token, the configured body-size cap, the resolved `<data_dir>/tmp`
/// and `<data_dir>/traces` paths, and a per-trace async mutex map
/// used to serialise concurrent batches for the same trace (so two
/// concurrent rename targets cannot pick the same `batch-NNNN`
/// filename).
pub struct AppState {
    pub expected_token: SecretString,
    pub max_body_bytes: u64,
    pub tmp_dir: PathBuf,
    pub traces_dir: PathBuf,
    /// One async mutex per `TraceKey` seen since startup. The outer
    /// `RwLock<HashMap<...>>` is read-locked on the cheap lookup
    /// path and write-locked only when a new trace first appears.
    /// The inner `tokio::sync::Mutex` is async-aware and is held
    /// across the rename + fsync `await`s. Mutex entries are not
    /// currently evicted; the finalize / retention slice will add
    /// that.
    pub trace_locks: RwLock<HashMap<TraceKey, Arc<AsyncMutex<()>>>>,
    /// Bounded ingest channel. The HTTP handler reserves a slot
    /// (via `try_reserve`) before committing the partial file; on
    /// commit success the handler sends a `BatchSubmission` and
    /// then responds `200`. On full → `503` per INV-7. Capacity is
    /// `config.server.queue_capacity`; the receiver task lives in
    /// `server::run` and exits when senders are dropped at
    /// graceful shutdown.
    pub batch_tx: mpsc::Sender<BatchSubmission>,
}

impl AppState {
    /// Look up (or create) the per-trace mutex. Uses an upgradeable
    /// pattern: try a read first, escalate to write only on miss.
    pub fn lock_for(&self, key: &TraceKey) -> Arc<AsyncMutex<()>> {
        if let Some(lock) = self
            .trace_locks
            .read()
            .expect("trace_locks not poisoned")
            .get(key)
        {
            return lock.clone();
        }
        let mut writer = self.trace_locks.write().expect("trace_locks not poisoned");
        writer
            .entry(key.clone())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }
}

pub type SharedState = Arc<AppState>;

/// Response extension that carries the number of body bytes the
/// ingest handler read from the wire (including the bytes read
/// before an oversize abort). The request-logging middleware reads
/// this off the response so it can include `body_bytes=<N>` in the
/// per-request log line without re-counting.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BodyBytes(pub u64);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_submission_debug_renders() {
        let sub = BatchSubmission {
            path: PathBuf::from("/var/lib/php-tree-viz/traces/abc.raw/batch-0001.msgpack"),
            trace_key: TraceKey::from_raw("abcdef0123456789abcdef0123456789"),
        };
        let rendered = format!("{sub:?}");
        assert!(rendered.contains("batch-0001.msgpack"));
        assert!(rendered.contains("abcdef0123456789abcdef0123456789"));
    }
}
