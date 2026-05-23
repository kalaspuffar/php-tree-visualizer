//! Periodic idle-finalize loop.
//!
//! Implements `SPECIFICATION.md` §2.2 sub-loop and the
//! `collector-finalize` capability. Ticks every
//! `config.finalize.tick_seconds`; on each tick, asks `Storage` for
//! active traces whose `last_batch_at_ns` precedes
//! `now - idle_seconds`, then calls `Storage::finalize_trace` on each
//! one and logs the outcome.
//!
//! The loop shares the same `Storage` instance as the decoder task
//! via an `Arc<tokio::sync::Mutex<Storage>>`. AD-1 ("storage is
//! single-threaded by design") still holds: the mutex serialises
//! decoder + finalize so only one task touches SQLite at a time.
//! Contention is negligible at the documented load (~25 MB/s peak,
//! a finalize pass is ≤ms per idle trace).

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;
use tokio::time::{interval, MissedTickBehavior};

use crate::storage::Storage;

/// Run the idle-finalize loop until the future is dropped (graceful
/// shutdown). Caller spawns this on the tokio runtime and never
/// awaits the handle — `axum::serve`'s shutdown future ending is
/// what tears down both the decoder and this loop together.
pub async fn run(storage: Arc<Mutex<Storage>>, idle_seconds: u32, tick_seconds: u32) {
    let mut ticker = interval(Duration::from_secs(tick_seconds as u64));
    // `Delay` (not the default `Burst`) prevents a thundering herd
    // after the loop has been blocked: missed ticks slip rather than
    // queue up. If the decoder ever holds the mutex past one tick
    // (it shouldn't — record_batch is ≤ms), we just resume the cadence
    // from the next wake-up instead of firing back-to-back ticks.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let idle_nanos: i64 = (idle_seconds as i64) * 1_000_000_000;

    loop {
        ticker.tick().await;
        run_one_tick(&storage, idle_nanos).await;
    }
}

/// One tick: list idle traces, finalize each, log per-trace
/// success / failure. Held as its own function so the unit tests can
/// drive a single tick without spawning the whole loop.
async fn run_one_tick(storage: &Mutex<Storage>, idle_nanos: i64) {
    let now_ns = now_realtime_ns();
    let cutoff_ns = now_ns - idle_nanos;

    let mut storage = storage.lock().await;
    let idle = match storage.list_idle_active_traces(cutoff_ns) {
        Ok(keys) => keys,
        Err(err) => {
            // The query failed before any per-trace work happened.
            // Log and yield — the next tick retries; nothing is left
            // half-done.
            eprintln!("finalize: list query failed: {err}");
            return;
        }
    };
    for key in idle {
        match storage.finalize_trace(&key, now_ns) {
            Ok(outcome) => {
                println!(
                    "finalized trace trace_key={} pending_dq2={} cpu_snapshot_available={}",
                    key,
                    outcome.pending_dq2,
                    i64::from(outcome.cpu_snapshot_available),
                );
            }
            Err(err) => {
                // One trace's failure doesn't kill the loop or the
                // tick. Surface it on stderr and move on to the
                // next idle trace.
                eprintln!("finalize: {err} trace_key={key}");
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
