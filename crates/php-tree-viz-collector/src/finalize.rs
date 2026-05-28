//! Periodic idle-finalize loop.
//!
//! Implements `SPECIFICATION.md` §2.2 sub-loop and the
//! `collector-finalize` capability. Ticks every
//! `config.finalize.tick_seconds`; on each tick, asks `Storage` for
//! active traces past the idle cutoff and applies the pending-aware
//! two-cutoff decision (per the `finalize-defers-on-pending` change):
//!
//! - `pending_count == 0`            → finalize cleanly (force=false)
//! - last batch older than the hard cap → force-finalize (force=true)
//! - otherwise                       → defer to a future tick
//!
//! The loop shares the same `Storage` instance as the decoder task
//! via an `Arc<tokio::sync::Mutex<Storage>>`. AD-1 ("storage is
//! single-threaded by design") still holds: the mutex serialises
//! decoder + finalize so only one task touches SQLite at a time.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;
use tokio::time::{interval, MissedTickBehavior};

use crate::storage::Storage;

/// Run the idle-finalize loop until the future is dropped (graceful
/// shutdown). Caller spawns this on the tokio runtime and never
/// awaits the handle — `axum::serve`'s shutdown future ending is
/// what tears down both the decoder and this loop together.
pub async fn run(
    storage: Arc<Mutex<Storage>>,
    idle_seconds: u32,
    tick_seconds: u32,
    max_pending_seconds: u32,
) {
    let mut ticker = interval(Duration::from_secs(tick_seconds as u64));
    // `Delay` (not the default `Burst`) prevents a thundering herd
    // after the loop has been blocked: missed ticks slip rather than
    // queue up. If the decoder ever holds the mutex past one tick
    // (it shouldn't — record_batch is ≤ms), we just resume the cadence
    // from the next wake-up instead of firing back-to-back ticks.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let idle_nanos: i64 = (idle_seconds as i64) * 1_000_000_000;
    let max_pending_nanos: i64 = (max_pending_seconds as i64) * 1_000_000_000;

    loop {
        ticker.tick().await;
        run_one_tick(&storage, idle_nanos, max_pending_nanos).await;
    }
}

/// One tick: list idle candidates, apply the two-cutoff decision per
/// candidate, finalize the ones whose cutoff fires, defer the rest.
/// Held as its own function so the unit tests can drive a single tick
/// without spawning the whole loop.
async fn run_one_tick(storage: &Mutex<Storage>, idle_nanos: i64, max_pending_nanos: i64) {
    let now_ns = now_realtime_ns();
    let idle_cutoff_ns = now_ns - idle_nanos;
    let hard_cutoff_ns = now_ns - max_pending_nanos;

    let mut storage = storage.lock().await;
    // The listing already filters to active traces past the idle
    // cutoff; for each we apply the pending-aware decision in Rust
    // using the `pending_count` and `last_batch_at_ns` columns
    // (`finalize-defers-on-pending`).
    let candidates = match storage.list_idle_active_traces(idle_cutoff_ns) {
        Ok(rows) => rows,
        Err(err) => {
            // The query failed before any per-trace work happened.
            // Log and yield — the next tick retries; nothing is left
            // half-done.
            tracing::warn!(reason = %err, "finalize list query failed");
            return;
        }
    };
    for c in candidates {
        // Clean finalize: no pending backlog — the trace is genuinely done.
        // Force-finalize: pending lingers past the hard cap — treat the
        //                 residual as orphans and emit DQ-1/DQ-2.
        // Defer:          pending non-empty AND within the hard cap —
        //                 resolving batches are presumed in flight.
        let force = if c.pending_count == 0 {
            false
        } else if c.last_batch_at_ns < hard_cutoff_ns {
            true
        } else {
            continue;
        };

        match storage.finalize_trace(&c.trace_key, now_ns, force) {
            Ok(outcome) => {
                tracing::info!(
                    trace_key = %c.trace_key,
                    force = force,
                    pending_dq1 = outcome.pending_dq1,
                    pending_dq2 = outcome.pending_dq2,
                    cpu_snapshot_available = outcome.cpu_snapshot_available,
                    raw_bytes_freed = outcome.raw_bytes_freed,
                    "trace finalized"
                );
            }
            Err(err) => {
                // One trace's failure doesn't kill the loop or the
                // tick. Surface it and move on to the next idle trace.
                tracing::warn!(
                    reason = %err,
                    trace_key = %c.trace_key,
                    "finalize failure"
                );
            }
        }
    }
}

fn now_realtime_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        // Pre-epoch system clocks are not a real-world case but the
        // arithmetic must not panic; fall back to 0, which means
        // "everything looks older than the cutoff" and the next
        // tick will retry.
        .unwrap_or(0)
}
