//! Binds the listener, prints the startup banner, runs axum, and
//! shuts down gracefully on SIGTERM or SIGINT.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::Mutex as AsyncMutex;

use super::{
    router, storage as http_storage, tmp, AppState, BatchSubmission, HttpError, SharedState,
};
use crate::config::Config;
use crate::finalize;
use crate::retention;
use crate::storage::Storage;

/// Entry point called from `main` after the config has been validated.
/// Returns only when the server stops (graceful shutdown → `Ok`,
/// any other failure → `Err`).
pub async fn run(config: Arc<Config>) -> Result<(), HttpError> {
    // `Config::validate` already asserted this parses and is loopback.
    let addr: SocketAddr = config
        .server
        .bind
        .parse()
        .expect("bind validated by Config::validate; parse cannot fail here");

    // Prepare the tmp + traces directories *before* binding so any
    // I/O failure here exits with status 3 before any client could
    // connect.
    let tmp_dir = tmp::ensure_clean_tmp_dir(&config.storage.data_dir)?;
    let traces_dir = http_storage::ensure_traces_dir(&config.storage.data_dir)?;

    // Open index.sqlite and prepare the per-trace connection map.
    // Failure here exits status 3 before the listener binds —
    // same family as bind / tmp-dir failures. The storage instance
    // moves into the decoder task below.
    let storage = Storage::open(&config.storage.data_dir, traces_dir.clone())
        .map_err(|source| HttpError::Storage { source })?;

    // Build the bounded ingest channel and spawn both the decoder
    // task and the idle-finalize loop. They share the same `Storage`
    // through `Arc<AsyncMutex<Storage>>` — rusqlite::Connection is
    // !Send, so we keep AD-1's single-task storage invariant by
    // serialising access through the mutex. Contention is negligible
    // at the documented load (decoder is ≤ms per batch, finalize fires
    // at most once per `tick_seconds`).
    //
    // When axum's serve future ends, the router and AppState are
    // dropped, the sender goes out of scope, `recv()` returns
    // `None`, the decoder task exits, and the finalize task is
    // cancelled by the runtime shutdown. No explicit shutdown
    // plumbing is needed for either.
    //
    // The decode + SQL writes run on the tokio runtime without
    // `spawn_blocking` — ~10 ms per batch is acceptable on the
    // async path. Revisit if profiling shows runtime starvation.
    let queue_capacity = config.server.queue_capacity as usize;
    let (batch_tx, mut batch_rx) = tokio::sync::mpsc::channel::<BatchSubmission>(queue_capacity);
    let storage = Arc::new(AsyncMutex::new(storage));
    let storage_for_decoder = storage.clone();
    let storage_for_finalize = storage.clone();
    let storage_for_retention = storage.clone();
    let storage_for_disk_usage = storage.clone();
    tokio::spawn(async move {
        while let Some(item) = batch_rx.recv().await {
            // INV-2 unaffected: `trace_key` is not a secret (it's
            // the on-disk path stem); the decoder never sees
            // header content.
            match tokio::fs::read(&item.path).await {
                Ok(bytes) => match crate::wire::parse_batch(&bytes) {
                    Ok(batch) => {
                        let received_at_ns = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_nanos() as i64)
                            .unwrap_or(0);
                        let result = {
                            // Scoped lock — released before the next
                            // `recv().await`, which is the only point
                            // the finalize loop can make progress.
                            let mut s = storage_for_decoder.lock().await;
                            s.record_batch(&item, &batch, received_at_ns)
                        };
                        match result {
                            Ok(outcome) => {
                                // F-1.10: structured `batch accepted`
                                // event carries the canonical trace
                                // identity (`trace_key`), the upstream
                                // `meta.trace_id`, `host`, `pid`,
                                // body counts, and the aggregation
                                // outcome fields. Body byte count is
                                // re-derived from the on-disk file
                                // size — same authority the receiver
                                // used to parse the batch.
                                let body_bytes = bytes.len() as u64;
                                tracing::info!(
                                    trace_key = %item.trace_key,
                                    trace_id = %batch.meta.trace_id,
                                    host = %batch.meta.host,
                                    pid = batch.meta.pid,
                                    body_bytes = body_bytes,
                                    dict_entries = batch.dict.len() as u64,
                                    call_count = batch.calls.len() as u64,
                                    nodes = outcome.nodes_touched,
                                    pending = outcome.pending_total,
                                    anomalies = outcome.anomalies_added,
                                    "batch accepted"
                                );
                            }
                            Err(e) => {
                                // Storage refused after a clean
                                // parse. Surface the same
                                // identity fields so the operator
                                // can correlate to the decoder log
                                // entry that did *not* land.
                                tracing::warn!(
                                    reason = %e,
                                    trace_key = %item.trace_key,
                                    trace_id = %batch.meta.trace_id,
                                    host = %batch.meta.host,
                                    pid = batch.meta.pid,
                                    "storage failure"
                                );
                            }
                        }
                    }
                    Err(err) => {
                        tracing::warn!(
                            reason = %err,
                            path = %item.path.display(),
                            trace_key = %item.trace_key,
                            "decoder failure"
                        );
                    }
                },
                Err(err) => {
                    tracing::warn!(
                        reason = %err,
                        path = %item.path.display(),
                        trace_key = %item.trace_key,
                        "decoder failure"
                    );
                }
            }
        }
    });

    // Idle-finalize loop. Lives alongside the decoder task and shares
    // `Storage` through the same `Arc<AsyncMutex<...>>`. Defaults
    // from §7.3: tick every 5s, finalize traces idle for ≥ 30s.
    tokio::spawn(finalize::run(
        storage_for_finalize,
        config.finalize.idle_seconds,
        config.finalize.tick_seconds,
    ));

    // Retention sweeper. Same shared `Storage`; ticks once per
    // `retention.tick_minutes` in production. The test-only
    // `tick_seconds` override (Option<u32>) wins when present,
    // letting integration tests drive the loop on a sub-minute
    // cadence. Default §7.3 tick is 60 minutes; effective tick is
    // in seconds for the loop body's tokio interval.
    let retention_tick_seconds: u64 = config
        .retention
        .tick_seconds
        .map(u64::from)
        .unwrap_or_else(|| u64::from(config.retention.tick_minutes) * 60);
    tokio::spawn(retention::run(
        storage_for_retention,
        config.storage.retention_days,
        retention_tick_seconds,
    ));

    // Disk-usage gauge. Shares the same `Storage` mutex as the
    // other three loops; lives long-as the runtime. Cadence is the
    // production default (one hour) unless an operator or test
    // overrides via the `observability` section. The first tick
    // fires immediately so the operator sees a baseline gauge
    // reading on startup; subsequent ticks honour the cadence.
    tokio::spawn(crate::observability::disk_usage_loop(
        storage_for_disk_usage,
        config.observability.clone(),
        config.storage.data_dir.clone(),
        config.storage.disk_capacity_bytes,
    ));

    let listener = TcpListener::bind(addr)
        .await
        .map_err(|source| HttpError::Bind { addr, source })?;
    let bound = listener
        .local_addr()
        .map_err(|source| HttpError::Bind { addr, source })?;

    // Install signal handlers *before* we announce readiness on
    // stdout. Otherwise a SIGTERM arriving in the gap between
    // "listening on …" and axum's first poll of the shutdown future
    // would hit the kernel default action — terminate — and the
    // process would exit with a signal status rather than `0`.
    let shutdown = build_shutdown_signal()?;

    tracing::info!(addr = %bound, "listening");
    notify_systemd_ready();

    let state: SharedState = Arc::new(AppState {
        expected_token: config.auth.token.clone(),
        max_body_bytes: config.server.max_body_bytes,
        tmp_dir,
        traces_dir,
        trace_locks: Default::default(),
        batch_tx,
    });

    let app = router::build(state).into_make_service_with_connect_info::<SocketAddr>();

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(HttpError::Serve)?;

    tracing::info!("shutdown complete");
    Ok(())
}

/// Install the SIGINT and SIGTERM handlers eagerly and return a
/// future that resolves on whichever fires first. The handlers are
/// installed by `signal::unix::signal(...)`'s side effect — once that
/// call returns, the default action (terminate) is replaced by
/// delivery to the tokio runtime.
#[cfg(unix)]
fn build_shutdown_signal() -> Result<impl Future<Output = ()>, HttpError> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigint = signal(SignalKind::interrupt()).map_err(HttpError::Serve)?;
    let mut sigterm = signal(SignalKind::terminate()).map_err(HttpError::Serve)?;
    Ok(async move {
        tokio::select! {
            _ = sigint.recv() => tracing::info!(signal = "SIGINT", "shutdown signal received"),
            _ = sigterm.recv() => tracing::info!(signal = "SIGTERM", "shutdown signal received"),
        }
    })
}

/// Non-Unix fallback — only SIGINT (Ctrl+C) is observed.
#[cfg(not(unix))]
fn build_shutdown_signal() -> Result<impl Future<Output = ()>, HttpError> {
    Ok(async move {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(reason = %e, "ctrl_c listener failed");
        }
        tracing::info!(signal = "SIGINT", "shutdown signal received");
    })
}

/// Send the systemd `READY=1` notification once the listener is bound,
/// matching the `Type=notify` directive in the tracked unit example.
///
/// Best-effort: failures log a `warn` event and return. The listener
/// is already up at this point; aborting now would be worse than
/// letting systemd's `TimeoutStartSec` handle the missed
/// notification. When `NOTIFY_SOCKET` is unset (the typical case
/// for ad-hoc invocations and the test suite) the function returns
/// silently — no logging, no syscalls beyond the env-var lookup.
///
/// Abstract-namespace `NOTIFY_SOCKET` values (path starts with `@`)
/// are not supported; the helper logs a `warn` and returns. systemd's
/// default on Debian uses regular filesystem paths under
/// `/run/systemd/notify`, which the regular branch handles.
#[cfg(unix)]
fn notify_systemd_ready() {
    let socket_value = match std::env::var_os("NOTIFY_SOCKET") {
        Some(v) if !v.is_empty() => v,
        _ => return,
    };
    notify_systemd_ready_inner(socket_value);
}

/// Pure-data implementation, factored out for unit testing without
/// touching the process environment.
#[cfg(unix)]
fn notify_systemd_ready_inner(socket_value: std::ffi::OsString) {
    let path_str = socket_value.to_string_lossy();
    if path_str.starts_with('@') {
        tracing::warn!(
            value = %path_str,
            "NOTIFY_SOCKET abstract namespace not supported; \
             systemd readiness signal skipped"
        );
        return;
    }
    let socket = match std::os::unix::net::UnixDatagram::unbound() {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(reason = %err, "could not open notify socket");
            return;
        }
    };
    if let Err(err) = socket.send_to(b"READY=1\n", std::path::Path::new(&socket_value)) {
        tracing::warn!(
            reason = %err,
            path = %path_str,
            "could not send READY=1 to systemd"
        );
    }
}

#[cfg(not(unix))]
fn notify_systemd_ready() {
    // Non-Unix targets don't have `AF_UNIX SOCK_DGRAM` in the form
    // systemd uses; nothing to send.
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Empty NOTIFY_SOCKET → silent no-op (mirrors the unset case,
    /// which the outer `notify_systemd_ready` filters before calling
    /// the inner). No tracing-subscriber capture needed — the only
    /// observable effect is that nothing happens.
    #[test]
    fn inner_no_op_on_at_prefix_does_not_panic() {
        // The inner function should return cleanly for abstract
        // namespace values. We don't assert on log output here (that's
        // covered by the integration test in tests/sd_notify.rs); we
        // just confirm the function returns without panic.
        notify_systemd_ready_inner(std::ffi::OsString::from("@example-abstract"));
    }

    /// Sending to a nonexistent path should warn-and-continue without
    /// panicking. Same no-panic contract.
    #[test]
    fn inner_send_failure_does_not_panic() {
        notify_systemd_ready_inner(std::ffi::OsString::from(
            "/tmp/phptv-nonexistent-notify-socket-for-test",
        ));
    }
}
