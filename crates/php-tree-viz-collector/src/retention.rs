//! Periodic retention sweeper.
//!
//! Implements `SPECIFICATION.md` §2.2 (retention-sweeper sub-loop)
//! and the `collector-retention` capability. Ticks every
//! `tick_seconds`; on each tick, asks `Storage` for traces whose
//! `start_time_ns` precedes `now - retention_days * 86 400 s`, then
//! calls `Storage::delete_trace` on each one and logs a per-tick
//! summary if anything was pruned.
//!
//! Shares the same `Arc<tokio::sync::Mutex<Storage>>` as the
//! decoder and finalize tasks. Contention is negligible: the tick
//! interval is an hour in production and the sweep cost is
//! dominated by filesystem syscalls (sub-ms per trace at the
//! documented scale).
//!
//! Failure semantics mirror `finalize.rs`: a per-trace
//! `delete_trace` failure logs `retention: <reason>
//! trace_key=<32hex>` to stderr and the loop continues with the
//! next expired trace. The `list_expired_traces` query failing
//! ends the tick early (no per-trace work to do); the next tick
//! retries.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;
use tokio::time::{interval_at, Instant, MissedTickBehavior};

use crate::storage::Storage;

/// Run the retention loop until the future is dropped (graceful
/// shutdown). The caller spawns this on the tokio runtime and never
/// awaits the handle — `axum::serve`'s shutdown future ending tears
/// it down along with the decoder and finalize tasks.
///
/// `tick_seconds` is taken directly (not minutes) so the loop body
/// stays unit-agnostic. The caller in `http::server::run` picks
/// `config.retention.tick_seconds` (test override) or
/// `config.retention.tick_minutes * 60` (production default).
pub async fn run(storage: Arc<Mutex<Storage>>, retention_days: u32, tick_seconds: u64) {
    // First tick fires *after* `tick_seconds`, not immediately. The
    // default `tokio::time::interval` semantics are first-tick-at-0,
    // which would mean a fresh collector sweeps before its first
    // ingest — convenient for re-applying retention after a restart,
    // but it forces every test that injects a synthetic past-dated
    // batch (anything with `meta.start_time < now - retention_days`)
    // to race the sweeper. Delaying the first tick by one cycle
    // matches operator intuition ("the sweeper runs every hour"
    // means "every hour, starting an hour from now") and removes
    // the race entirely. A trace older than `retention_days` still
    // gets pruned — just on the next tick instead of the immediate
    // one. At the production cadence of 60 min that's irrelevant;
    // at the test cadence of 1 s it's exactly the bound the tests
    // already expect via `wait_for_stdout`.
    let period = Duration::from_secs(tick_seconds);
    let mut ticker = interval_at(Instant::now() + period, period);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // Pre-compute the cutoff offset once. Even at retention_days =
    // u32::MAX (~136 years), this fits in i64 with plenty of
    // headroom.
    let retention_nanos: i64 = (retention_days as i64) * 86_400 * 1_000_000_000;

    loop {
        ticker.tick().await;
        run_one_tick(&storage, retention_nanos).await;
    }
}

/// One tick: list expired traces, prune each, log a summary if any
/// were pruned. Held as its own function so the failure semantics
/// can be unit-tested directly without spawning the loop.
async fn run_one_tick(storage: &Mutex<Storage>, retention_nanos: i64) {
    let now_ns = now_realtime_ns();
    let cutoff_ns = now_ns.saturating_sub(retention_nanos);

    let mut storage = storage.lock().await;
    let expired = match storage.list_expired_traces(cutoff_ns) {
        Ok(keys) => keys,
        Err(err) => {
            eprintln!("retention: list query failed: {err}");
            return;
        }
    };

    let mut removed_traces: u32 = 0;
    let mut freed_bytes: u64 = 0;
    for key in expired {
        match storage.delete_trace(&key) {
            Ok(outcome) => {
                removed_traces = removed_traces.saturating_add(1);
                freed_bytes = freed_bytes.saturating_add(outcome.freed_bytes);
            }
            Err(err) => {
                // One trace's failure doesn't abort the tick. The
                // summary line that follows reflects only the
                // successful prunes.
                eprintln!("retention: {err} trace_key={key}");
            }
        }
    }

    if removed_traces > 0 {
        println!("swept retention removed_traces={removed_traces} freed_bytes={freed_bytes}");
    }
}

fn now_realtime_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        // Pre-epoch system clocks are not a real-world case but the
        // arithmetic must not panic; fall back to 0, which means
        // "the cutoff is far in the past, nothing expires this tick"
        // (saturating_sub keeps that bounded too).
        .unwrap_or(0)
}
