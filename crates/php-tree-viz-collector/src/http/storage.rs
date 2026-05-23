//! Filesystem persistence for the ingest path.
//!
//! `commit_partial` is the durability core: it picks the next
//! `batch-NNNN.msgpack` filename for the trace, atomically renames
//! the streamed partial into place, fsyncs the file, and (on Unix)
//! fsyncs the parent directory so the rename's directory-entry
//! change is durable. INV-1: `200 OK` is only returned after this
//! function has returned `Ok`.

use std::path::{Path, PathBuf};

use tokio::fs::File;

use super::HttpError;
use crate::tracekey::TraceKey;

const BATCH_PREFIX: &str = "batch-";
const BATCH_EXT: &str = "msgpack";
/// `SPECIFICATION.md` §4.4.1: 4-digit zero-padded counter; cap at
/// 9999 for now. The proper §4.4.2 overflow naming
/// (`batch-9999.NNNNN.msgpack`) is a future change.
const BATCH_MAX: u32 = 9999;

/// Ensure `<data_dir>/traces/` exists at startup with mode `0o700`
/// on Unix, mirroring the tmp-dir contract.
pub fn ensure_traces_dir(data_dir: &Path) -> Result<PathBuf, HttpError> {
    let traces_dir = data_dir.join("traces");
    std::fs::create_dir_all(&traces_dir).map_err(|source| HttpError::TracesDir {
        path: traces_dir.clone(),
        source,
    })?;
    set_dir_perms(&traces_dir)?;
    Ok(traces_dir)
}

#[cfg(unix)]
fn set_dir_perms(dir: &Path) -> Result<(), HttpError> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o700);
    std::fs::set_permissions(dir, perms).map_err(|source| HttpError::TracesDir {
        path: dir.to_path_buf(),
        source,
    })
}

#[cfg(not(unix))]
fn set_dir_perms(_dir: &Path) -> Result<(), HttpError> {
    Ok(())
}

/// Commit a streamed partial file to its canonical location.
///
/// Caller invariant: the per-trace mutex from `AppState::lock_for`
/// is held for the duration of this call, so two concurrent batches
/// for the same trace cannot pick the same `batch-NNNN` filename.
///
/// Returns the canonical path on success.
pub async fn commit_partial(
    partial: &Path,
    traces_dir: &Path,
    key: &TraceKey,
) -> Result<PathBuf, HttpError> {
    let trace_dir = traces_dir.join(format!("{}.raw", key.as_str()));
    ensure_trace_dir(&trace_dir).await?;

    let next = next_batch_number(&trace_dir).await?;
    if next > BATCH_MAX {
        return Err(HttpError::TraceFull { key: key.clone() });
    }
    let target = trace_dir.join(format!("{BATCH_PREFIX}{next:04}.{BATCH_EXT}"));

    tokio::fs::rename(partial, &target)
        .await
        .map_err(|source| HttpError::Rename {
            from: partial.to_path_buf(),
            to: target.clone(),
            source,
        })?;

    fsync_path(&target).await?;
    #[cfg(unix)]
    fsync_path(&trace_dir).await?;

    Ok(target)
}

async fn ensure_trace_dir(trace_dir: &Path) -> Result<(), HttpError> {
    tokio::fs::create_dir_all(trace_dir)
        .await
        .map_err(|source| HttpError::TracesDir {
            path: trace_dir.to_path_buf(),
            source,
        })?;
    #[cfg(unix)]
    {
        // Tighten to 0o700 only the first time we see this trace.
        // Subsequent calls are no-ops because the mode is already
        // correct; calling set_permissions repeatedly is cheap.
        let path = trace_dir.to_path_buf();
        tokio::task::spawn_blocking(move || {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
        })
        .await
        .map_err(|join| HttpError::TracesDir {
            path: trace_dir.to_path_buf(),
            source: std::io::Error::other(join),
        })?
        .map_err(|source| HttpError::TracesDir {
            path: trace_dir.to_path_buf(),
            source,
        })?;
    }
    Ok(())
}

/// Find the next `batch-NNNN` number for a trace's raw directory.
///
/// Returns the max existing `NNNN` + 1, or `1` if the directory is
/// empty / contains no matching files. Non-matching files (anything
/// not `batch-<4 digits>.msgpack`) are ignored.
pub async fn next_batch_number(trace_dir: &Path) -> Result<u32, HttpError> {
    let mut entries =
        tokio::fs::read_dir(trace_dir)
            .await
            .map_err(|source| HttpError::TracesDir {
                path: trace_dir.to_path_buf(),
                source,
            })?;

    let mut max_seen: u32 = 0;
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|source| HttpError::TracesDir {
            path: trace_dir.to_path_buf(),
            source,
        })?
    {
        let name = entry.file_name();
        let name = match name.to_str() {
            Some(s) => s,
            None => continue,
        };
        if let Some(n) = parse_batch_number(name) {
            if n > max_seen {
                max_seen = n;
            }
        }
    }
    Ok(max_seen + 1)
}

/// Parse `batch-NNNN.msgpack` → `Some(NNNN)`, anything else → None.
fn parse_batch_number(name: &str) -> Option<u32> {
    let stem = name.strip_suffix(".msgpack")?;
    let digits = stem.strip_prefix(BATCH_PREFIX)?;
    if digits.len() != 4 {
        return None;
    }
    digits.parse().ok()
}

async fn fsync_path(path: &Path) -> Result<(), HttpError> {
    let file = File::open(path).await.map_err(|source| HttpError::Fsync {
        path: path.to_path_buf(),
        source,
    })?;
    file.sync_all().await.map_err(|source| HttpError::Fsync {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "phptv-storage-{}-{}-{}",
            std::process::id(),
            label,
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parse_batch_number_accepts_well_formed() {
        assert_eq!(parse_batch_number("batch-0001.msgpack"), Some(1));
        assert_eq!(parse_batch_number("batch-0042.msgpack"), Some(42));
        assert_eq!(parse_batch_number("batch-9999.msgpack"), Some(9999));
    }

    #[test]
    fn parse_batch_number_rejects_malformed() {
        assert_eq!(parse_batch_number("batch-1.msgpack"), None); // not 4 digits
        assert_eq!(parse_batch_number("batch-99999.msgpack"), None); // 5 digits
        assert_eq!(parse_batch_number("batch-abcd.msgpack"), None); // non-digits
        assert_eq!(parse_batch_number("file-0001.msgpack"), None); // wrong prefix
        assert_eq!(parse_batch_number("batch-0001.txt"), None); // wrong extension
        assert_eq!(parse_batch_number("batch-0001"), None); // no extension
    }

    #[tokio::test]
    async fn next_batch_number_returns_one_for_empty_dir() {
        let dir = unique_dir("empty");
        let n = next_batch_number(&dir).await.unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn next_batch_number_returns_max_plus_one() {
        let dir = unique_dir("max_plus_one");
        std::fs::write(dir.join("batch-0001.msgpack"), b"").unwrap();
        std::fs::write(dir.join("batch-0002.msgpack"), b"").unwrap();
        std::fs::write(dir.join("batch-0005.msgpack"), b"").unwrap();
        let n = next_batch_number(&dir).await.unwrap();
        assert_eq!(n, 6);
    }

    #[tokio::test]
    async fn next_batch_number_ignores_unrelated_files() {
        let dir = unique_dir("ignores_others");
        std::fs::write(dir.join("batch-0001.msgpack"), b"").unwrap();
        std::fs::write(dir.join("README.md"), b"").unwrap();
        std::fs::write(dir.join("batch-0001.partial"), b"").unwrap();
        std::fs::write(dir.join("notes.txt"), b"").unwrap();
        let n = next_batch_number(&dir).await.unwrap();
        assert_eq!(n, 2);
    }

    #[tokio::test]
    async fn commit_partial_renames_and_succeeds() {
        let data_dir = unique_dir("commit_ok");
        let tmp_dir = data_dir.join("tmp");
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let traces_dir = ensure_traces_dir(&data_dir).unwrap();

        let partial = tmp_dir.join("abc.partial");
        std::fs::write(&partial, b"hello").unwrap();

        let key = TraceKey::from_raw("0000000000000000000000000000beef");
        let target = commit_partial(&partial, &traces_dir, &key).await.unwrap();

        assert!(!partial.exists(), "partial should be renamed away");
        assert!(target.exists(), "target should exist");
        assert!(target.ends_with("batch-0001.msgpack"));
        assert_eq!(std::fs::read(&target).unwrap(), b"hello");
    }

    #[tokio::test]
    async fn commit_partial_increments_on_existing_batch() {
        let data_dir = unique_dir("commit_increment");
        let tmp_dir = data_dir.join("tmp");
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let traces_dir = ensure_traces_dir(&data_dir).unwrap();

        let key = TraceKey::from_raw("00000000000000000000000000000abc");
        let trace_dir = traces_dir.join(format!("{}.raw", key.as_str()));
        std::fs::create_dir_all(&trace_dir).unwrap();
        std::fs::write(trace_dir.join("batch-0001.msgpack"), b"earlier").unwrap();

        let partial = tmp_dir.join("p.partial");
        std::fs::write(&partial, b"next").unwrap();
        let target = commit_partial(&partial, &traces_dir, &key).await.unwrap();

        assert!(target.ends_with("batch-0002.msgpack"));
    }

    #[tokio::test]
    async fn commit_partial_returns_trace_full_at_9999() {
        let data_dir = unique_dir("commit_full");
        let tmp_dir = data_dir.join("tmp");
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let traces_dir = ensure_traces_dir(&data_dir).unwrap();

        let key = TraceKey::from_raw("00000000000000000000000000000def");
        let trace_dir = traces_dir.join(format!("{}.raw", key.as_str()));
        std::fs::create_dir_all(&trace_dir).unwrap();
        std::fs::write(trace_dir.join("batch-9999.msgpack"), b"last").unwrap();

        let partial = tmp_dir.join("p.partial");
        std::fs::write(&partial, b"over").unwrap();

        let err = commit_partial(&partial, &traces_dir, &key)
            .await
            .expect_err("must report trace_full");
        assert!(matches!(err, HttpError::TraceFull { .. }));
        // Partial is NOT auto-deleted at this layer; the caller
        // decides whether to retain or delete on the trace_full
        // path. We just assert the rename did not happen.
        assert!(partial.exists());
    }

    #[tokio::test]
    async fn ensure_traces_dir_is_idempotent() {
        let data_dir = unique_dir("ensure_idempotent");
        let d1 = ensure_traces_dir(&data_dir).unwrap();
        let d2 = ensure_traces_dir(&data_dir).unwrap();
        assert_eq!(d1, d2);
        assert!(d1.is_dir());
    }
}
