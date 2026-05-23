//! Tmp-file machinery for the streaming ingest path.
//!
//! - `ensure_clean_tmp_dir` runs once at startup. It creates the
//!   `<data_dir>/tmp/` directory (idempotent), sets its mode to
//!   `0o700` on Unix, and deletes any `*.partial` files left behind
//!   by a previous run — `SPECIFICATION.md` §4.4.4 ("`tmp/*.partial`
//!   is deleted at startup — anything there did not survive the
//!   fsync rename").
//! - `make_filename` returns a 32-character lowercase-hex stem that
//!   is unique per process: 16 hex chars of `SystemTime` nanos, 8
//!   of process id, 8 of an atomic counter. No external crate
//!   needed, no cryptographic randomness required (these are tmp
//!   file names, not secrets).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::HttpError;

/// Create `<data_dir>/tmp/` (idempotent), tighten its mode to
/// `0o700` on Unix, and remove any pre-existing `*.partial` files.
/// Returns the resolved tmp path on success.
pub fn ensure_clean_tmp_dir(data_dir: &Path) -> Result<PathBuf, HttpError> {
    let tmp_dir = data_dir.join("tmp");
    std::fs::create_dir_all(&tmp_dir).map_err(|source| HttpError::TmpDir {
        path: tmp_dir.clone(),
        source,
    })?;
    set_tmp_dir_permissions(&tmp_dir)?;
    drain_partial_files(&tmp_dir)?;
    Ok(tmp_dir)
}

#[cfg(unix)]
fn set_tmp_dir_permissions(tmp_dir: &Path) -> Result<(), HttpError> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o700);
    std::fs::set_permissions(tmp_dir, perms).map_err(|source| HttpError::TmpDir {
        path: tmp_dir.to_path_buf(),
        source,
    })
}

#[cfg(not(unix))]
fn set_tmp_dir_permissions(_tmp_dir: &Path) -> Result<(), HttpError> {
    // No POSIX permissions to set; whatever the parent directory's
    // ACL inheritance yields is what we get. Documented in design.
    Ok(())
}

fn drain_partial_files(tmp_dir: &Path) -> Result<(), HttpError> {
    let entries = std::fs::read_dir(tmp_dir).map_err(|source| HttpError::TmpDir {
        path: tmp_dir.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| HttpError::TmpDir {
            path: tmp_dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "partial") {
            // Best-effort delete; a partial file we cannot remove
            // is the operator's problem and surfaces at the next
            // ingest attempt that targets the same name (which is
            // statistically impossible — see `make_filename`).
            if let Err(source) = std::fs::remove_file(&path) {
                return Err(HttpError::TmpDir { path, source });
            }
        }
    }
    Ok(())
}

/// Generate a 32-character lowercase-hex file stem.
///
/// Layout (high-to-low bits):
///   - 16 hex chars (64 bits): low 64 bits of `SystemTime` nanos
///   - 8  hex chars (32 bits): `std::process::id()`
///   - 8  hex chars (32 bits): per-process atomic counter
///
/// Within one process, the counter alone guarantees uniqueness.
/// Across processes, the pid bits separate them. Across restarts,
/// the nanos bits separate them. No cryptographic property is
/// claimed — collisions are statistically impossible in any
/// realistic deployment.
pub fn make_filename() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed) as u32;
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{nanos:016x}{pid:08x}{counter:08x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "phptv-tmp-mod-{}-{}-{}",
            std::process::id(),
            label,
            n
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn make_filename_is_32_lowercase_hex() {
        let f = make_filename();
        assert_eq!(f.len(), 32, "filename {f:?} is not 32 chars");
        assert!(
            f.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "non-lowercase-hex character in {f:?}"
        );
    }

    #[test]
    fn make_filename_is_unique_across_1000_calls() {
        let mut seen = HashSet::with_capacity(1000);
        for _ in 0..1000 {
            assert!(seen.insert(make_filename()));
        }
    }

    #[test]
    fn ensure_clean_tmp_dir_creates_the_subdir_if_absent() {
        let data_dir = unique_dir("creates_subdir");
        std::fs::create_dir_all(&data_dir).unwrap();

        let tmp = ensure_clean_tmp_dir(&data_dir).expect("ensure should succeed");
        assert_eq!(tmp, data_dir.join("tmp"));
        assert!(tmp.is_dir(), "tmp not created");
    }

    #[test]
    fn ensure_clean_tmp_dir_deletes_pre_existing_partial_files() {
        let data_dir = unique_dir("deletes_partials");
        let tmp = data_dir.join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let leftover = tmp.join("0123456789abcdef0123456789abcdef.partial");
        std::fs::write(&leftover, b"old data").unwrap();
        assert!(leftover.exists());

        ensure_clean_tmp_dir(&data_dir).expect("ensure should succeed");
        assert!(!leftover.exists(), "partial file not removed");
    }

    #[test]
    fn ensure_clean_tmp_dir_preserves_non_partial_files() {
        let data_dir = unique_dir("preserves_other");
        let tmp = data_dir.join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let keeper = tmp.join("keep-me.txt");
        std::fs::write(&keeper, b"please").unwrap();

        ensure_clean_tmp_dir(&data_dir).expect("ensure should succeed");
        assert!(keeper.exists(), "non-partial file was removed");
    }

    #[cfg(unix)]
    #[test]
    fn ensure_clean_tmp_dir_sets_0o700_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let data_dir = unique_dir("chmod");
        std::fs::create_dir_all(data_dir.join("tmp")).unwrap();
        // Loosen to 0o755 first to verify the function tightens it.
        std::fs::set_permissions(data_dir.join("tmp"), std::fs::Permissions::from_mode(0o755))
            .unwrap();

        let tmp = ensure_clean_tmp_dir(&data_dir).unwrap();
        let mode = std::fs::metadata(&tmp).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "mode {mode:o} != 0o700");
    }

    #[test]
    fn ensure_clean_tmp_dir_returns_error_when_parent_is_a_file() {
        // Create a regular file at `data_dir`; we then ask to create
        // `<data_dir>/tmp` underneath it, which can't work.
        let data_dir = unique_dir("parent_is_file");
        let parent = data_dir.parent().unwrap();
        std::fs::create_dir_all(parent).unwrap();
        std::fs::write(&data_dir, b"not a dir").unwrap();

        let err = ensure_clean_tmp_dir(&data_dir).expect_err("must fail");
        let rendered = format!("{err}");
        assert!(
            rendered.contains(data_dir.to_str().unwrap()),
            "error must name the path: {rendered}"
        );
    }
}
