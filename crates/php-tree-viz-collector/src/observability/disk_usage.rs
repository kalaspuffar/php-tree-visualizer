//! Periodic disk-usage gauge.
//!
//! Owned by the `collector-observability` capability. One async task,
//! spawned alongside the decoder / finalize / retention loops by
//! `http::server::run`. Wakes on a configurable cadence
//! (`observability.disk_usage_tick_seconds`, default 1 h), measures
//! the total bytes used under `<config.storage.data_dir>` by
//! summing the documented file layout, and emits one structured
//! event per tick.
//!
//! Why a custom walk rather than `df`: an operator may co-locate the
//! collector with other services on a shared volume — `df` would
//! include their bytes. We measure what we control.
//!
//! Why the layout filter: a stray operator-placed file inside
//! `<data_dir>/` (a backup, a README, a tarball under `traces/`)
//! must not be counted toward the gauge. The filter lists the exact
//! shapes the collector itself writes; anything else is ignored.
//!
//! Why we don't checkpoint SQLite WAL first: that would race the
//! decoder. We accept that `*.sqlite-wal` may be momentarily larger
//! than steady-state — the `over_threshold` semantic compares
//! *total bytes*, so transient WAL excess is the right thing to count.
//! (See COMMENTS.md line 451 for the related hazard in delete-path
//! accounting.)

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::{interval, MissedTickBehavior};
use walkdir::WalkDir;

use crate::config::Observability;
use crate::storage::Storage;

/// Run the disk-usage gauge until the future is dropped. Matches the
/// shape of `finalize::run` and `retention::run` so the spawn site
/// stays uniform.
pub async fn disk_usage_loop(
    storage: Arc<Mutex<Storage>>,
    config: Observability,
    data_dir: PathBuf,
    disk_capacity_bytes: Option<u64>,
) {
    let period = effective_tick(&config);
    let mut ticker = interval(period);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // The first `tick()` resolves immediately under tokio's default;
    // emit one event right away so operators see a baseline on
    // startup. Subsequent ticks honour the configured cadence.

    loop {
        ticker.tick().await;
        run_one_tick(&storage, &data_dir, &config, disk_capacity_bytes).await;
    }
}

async fn run_one_tick(
    storage: &Mutex<Storage>,
    data_dir: &Path,
    config: &Observability,
    disk_capacity_bytes: Option<u64>,
) {
    // Short lock: read trace_count and release before the FS walk so
    // the decoder is not blocked behind a directory traversal.
    let trace_count = {
        let storage = storage.lock().await;
        match storage.count_traces() {
            Ok(n) => n,
            Err(err) => {
                tracing::warn!(reason = %err, "disk usage count_traces failed");
                return;
            }
        }
    };

    let data_dir_bytes = match measure_data_dir(data_dir) {
        Ok(n) => n,
        Err(err) => {
            tracing::warn!(reason = %err, "disk usage measurement failed");
            return;
        }
    };

    let over_threshold = is_over_threshold(
        data_dir_bytes,
        disk_capacity_bytes,
        config.disk_usage_warn_pct,
    );

    if over_threshold {
        tracing::warn!(
            data_dir_bytes,
            trace_count,
            threshold_pct = config.disk_usage_warn_pct,
            over_threshold = true,
            "disk usage"
        );
    } else {
        tracing::info!(
            data_dir_bytes,
            trace_count,
            threshold_pct = config.disk_usage_warn_pct,
            over_threshold = false,
            "disk usage"
        );
    }
}

/// Effective tick interval — the test-only override wins when set,
/// matching the retention pattern in `config::Retention`.
fn effective_tick(config: &Observability) -> Duration {
    Duration::from_secs(
        config
            .disk_usage_tick_seconds_test_override
            .unwrap_or(config.disk_usage_tick_seconds),
    )
}

/// Sum on-disk bytes under `data_dir` for files matching the
/// documented collector layout. Files outside the layout (operator
/// surprises) are ignored.
pub fn measure_data_dir(data_dir: &Path) -> std::io::Result<u64> {
    let mut total: u64 = 0;
    for entry in WalkDir::new(data_dir).into_iter().filter_map(|r| r.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        if !path_is_in_layout(data_dir, entry.path()) {
            continue;
        }
        match entry.metadata() {
            Ok(meta) => total = total.saturating_add(meta.len()),
            // A file vanishing between walkdir's listing and our
            // metadata call is normal during retention. Skip it.
            Err(_) => continue,
        }
    }
    Ok(total)
}

/// Does the path match the layout the collector itself writes?
///
/// Layout (relative to `data_dir`):
/// - `index.sqlite{,-wal,-shm}`
/// - `traces/*.sqlite{,-wal,-shm}`
/// - `traces/*.raw/*`
/// - `tmp/*`
fn path_is_in_layout(data_dir: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(data_dir) else {
        return false;
    };
    let mut components = rel.components();
    let Some(first) = components.next() else {
        return false;
    };
    match first.as_os_str().to_str() {
        Some(name) if is_index_sqlite_file(name) && components.next().is_none() => true,
        Some("traces") => {
            let Some(second) = components.next() else {
                return false;
            };
            let Some(child_name) = second.as_os_str().to_str() else {
                return false;
            };
            if is_per_trace_sqlite(child_name) && components.next().is_none() {
                return true;
            }
            // A `.raw/` directory: any file directly under it is in
            // layout. (Nested subdirectories are not part of the
            // documented layout.)
            if let Some(stem) = child_name.strip_suffix(".raw") {
                if !stem.is_empty() && components.clone().count() == 1 {
                    return true;
                }
            }
            false
        }
        Some("tmp") => {
            // Any regular file directly under `tmp/` is in layout
            // (partial files are tracked). Nested directories are not.
            components.next().is_some() && components.next().is_none()
        }
        _ => false,
    }
}

fn is_index_sqlite_file(name: &str) -> bool {
    matches!(
        name,
        "index.sqlite" | "index.sqlite-wal" | "index.sqlite-shm"
    )
}

fn is_per_trace_sqlite(name: &str) -> bool {
    // `<32hex>.sqlite{,-wal,-shm}`. We don't validate hex strictly
    // (any name ending in `.sqlite{,-wal,-shm}` directly under
    // `traces/` counts); strict hex validation would reject legitimate
    // future formats and isn't required by the spec.
    name.ends_with(".sqlite") || name.ends_with(".sqlite-wal") || name.ends_with(".sqlite-shm")
}

fn is_over_threshold(data_dir_bytes: u64, capacity: Option<u64>, warn_pct: u8) -> bool {
    let Some(capacity) = capacity else {
        return false;
    };
    if capacity == 0 || warn_pct == 0 {
        return false;
    }
    // Threshold: data_dir_bytes * 100 >= capacity * warn_pct.
    // Use u128 to dodge multiplication overflow when both sides are large.
    let lhs = (data_dir_bytes as u128).saturating_mul(100);
    let rhs = (capacity as u128).saturating_mul(warn_pct as u128);
    lhs >= rhs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn over_threshold_false_when_capacity_unset() {
        assert!(!is_over_threshold(1_000_000, None, 80));
    }

    #[test]
    fn over_threshold_true_when_above_pct() {
        assert!(is_over_threshold(800, Some(1000), 80));
        assert!(is_over_threshold(801, Some(1000), 80));
    }

    #[test]
    fn over_threshold_false_when_below_pct() {
        assert!(!is_over_threshold(799, Some(1000), 80));
    }

    #[test]
    fn over_threshold_handles_max_values_without_overflow() {
        // Would overflow u64 multiplication but u128 holds it.
        assert!(is_over_threshold(u64::MAX, Some(u64::MAX), 100));
    }

    #[test]
    fn over_threshold_pct_zero_is_never_over() {
        assert!(!is_over_threshold(u64::MAX, Some(u64::MAX), 0));
    }

    #[test]
    fn measure_data_dir_ignores_non_layout_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("traces")).unwrap();
        fs::create_dir_all(root.join("traces/aaaa.raw")).unwrap();
        fs::create_dir_all(root.join("tmp")).unwrap();

        // In layout:
        fs::write(root.join("index.sqlite"), b"abcdef").unwrap(); // 6 bytes
        fs::write(root.join("index.sqlite-wal"), b"wal").unwrap(); // 3
        fs::write(root.join("traces/aaaa.sqlite"), b"sqlite").unwrap(); // 6
        fs::write(root.join("traces/aaaa.raw/batch-0001.msgpack"), b"raw1").unwrap(); // 4
        fs::write(root.join("tmp/x.partial"), b"par").unwrap(); // 3

        // Out of layout:
        fs::write(root.join("traces/spurious.txt"), b"BIG_BIG_BIG").unwrap();
        fs::write(root.join("rogue.log"), b"NOOOOOOOOOOOOOOOOOOOOOOOO").unwrap();

        let total = measure_data_dir(root).unwrap();
        assert_eq!(total, 6 + 3 + 6 + 4 + 3);
    }

    #[test]
    fn measure_data_dir_handles_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(measure_data_dir(tmp.path()).unwrap(), 0);
    }

    #[test]
    fn measure_data_dir_handles_missing_dir() {
        // Missing data_dir is treated as zero (walkdir's error is
        // swallowed by `filter_map(|r| r.ok())` because we'd rather
        // log a zero gauge than crash the loop).
        let total = measure_data_dir(Path::new("/definitely/not/here")).unwrap();
        assert_eq!(total, 0);
    }

    #[test]
    fn effective_tick_uses_override_when_set() {
        let cfg = Observability {
            disk_usage_tick_seconds: 3600,
            disk_usage_warn_pct: 80,
            disk_usage_tick_seconds_test_override: Some(2),
        };
        assert_eq!(effective_tick(&cfg), Duration::from_secs(2));
    }

    #[test]
    fn effective_tick_uses_config_when_no_override() {
        let cfg = Observability {
            disk_usage_tick_seconds: 7,
            disk_usage_warn_pct: 80,
            disk_usage_tick_seconds_test_override: None,
        };
        assert_eq!(effective_tick(&cfg), Duration::from_secs(7));
    }
}
