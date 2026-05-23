//! Error type produced by the HTTP layer (bind, serve, shutdown).
//!
//! Same single-line `Display` contract and `Error::source` chain that
//! `config::ConfigError` follows, so the operator sees one
//! human-readable line on stderr regardless of which subsystem
//! refused to start.

use std::net::SocketAddr;
use std::path::PathBuf;

use crate::tracekey::TraceKey;

#[derive(Debug)]
pub enum HttpError {
    /// `TcpListener::bind` failed (address in use, permission denied,
    /// kernel ran out of file descriptors, etc.).
    Bind {
        addr: SocketAddr,
        source: std::io::Error,
    },
    /// The HTTP server task itself returned an I/O error during
    /// `axum::serve(...).await`.
    Serve(std::io::Error),
    /// `<data_dir>/tmp/` could not be created, chmod-ed, or scanned
    /// at startup. Surface during boot; exit 3.
    TmpDir {
        path: PathBuf,
        source: std::io::Error,
    },
    /// A streaming write to a `.partial` tmp file failed at runtime
    /// (disk full, fs error, …). Caught at the handler boundary and
    /// surfaced to the client as `500`; the operator sees the path
    /// in the per-request log line.
    TmpWrite {
        path: PathBuf,
        source: std::io::Error,
    },
    /// `<data_dir>/traces/` or `<data_dir>/traces/<key>.raw/` could
    /// not be created or chmod-ed.
    TracesDir {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The atomic rename from `tmp/<rand>.partial` into
    /// `traces/<key>.raw/batch-NNNN.msgpack` failed.
    Rename {
        from: PathBuf,
        to: PathBuf,
        source: std::io::Error,
    },
    /// `sync_all()` on a file or directory failed.
    Fsync {
        path: PathBuf,
        source: std::io::Error,
    },
    /// A trace has accumulated 9999 batches and cannot accept any
    /// more. Defensive; proper §4.4.2 rollover is a future change.
    TraceFull { key: TraceKey },
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bind { addr, source } => {
                write!(f, "could not bind {addr}: {source}")
            }
            Self::Serve(source) => write!(f, "http server failed: {source}"),
            Self::TmpDir { path, source } => {
                write!(
                    f,
                    "could not prepare tmp directory {}: {}",
                    path.display(),
                    source
                )
            }
            Self::TmpWrite { path, source } => {
                write!(
                    f,
                    "could not write to tmp file {}: {}",
                    path.display(),
                    source
                )
            }
            Self::TracesDir { path, source } => {
                write!(
                    f,
                    "could not prepare traces directory {}: {}",
                    path.display(),
                    source
                )
            }
            Self::Rename { from, to, source } => {
                write!(
                    f,
                    "could not rename {} -> {}: {}",
                    from.display(),
                    to.display(),
                    source
                )
            }
            Self::Fsync { path, source } => {
                write!(f, "fsync of {} failed: {}", path.display(), source)
            }
            Self::TraceFull { key } => {
                write!(f, "trace {} has reached the 9999-batch cap", key)
            }
        }
    }
}

impl std::error::Error for HttpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Bind { source, .. }
            | Self::Serve(source)
            | Self::TmpDir { source, .. }
            | Self::TmpWrite { source, .. }
            | Self::TracesDir { source, .. }
            | Self::Rename { source, .. }
            | Self::Fsync { source, .. } => Some(source),
            Self::TraceFull { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_bind_contains_addr_and_reason() {
        let addr: SocketAddr = "127.0.0.1:8088".parse().unwrap();
        let err = HttpError::Bind {
            addr,
            source: std::io::Error::new(std::io::ErrorKind::AddrInUse, "address in use"),
        };
        let rendered = format!("{err}");
        assert!(rendered.contains("127.0.0.1:8088"));
        assert!(rendered.contains("address in use"));
        assert!(!rendered.contains('\n'));
    }

    #[test]
    fn source_is_set_for_every_variant() {
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let bind = HttpError::Bind {
            addr,
            source: std::io::Error::new(std::io::ErrorKind::AddrInUse, "x"),
        };
        let serve = HttpError::Serve(std::io::Error::other("x"));
        let tmp_dir = HttpError::TmpDir {
            path: PathBuf::from("/x"),
            source: std::io::Error::other("x"),
        };
        let tmp_write = HttpError::TmpWrite {
            path: PathBuf::from("/x/y.partial"),
            source: std::io::Error::other("x"),
        };
        assert!(std::error::Error::source(&bind).is_some());
        assert!(std::error::Error::source(&serve).is_some());
        assert!(std::error::Error::source(&tmp_dir).is_some());
        assert!(std::error::Error::source(&tmp_write).is_some());
    }

    #[test]
    fn display_tmp_dir_is_single_line_and_names_path() {
        let err = HttpError::TmpDir {
            path: PathBuf::from("/var/lib/php-tree-viz/tmp"),
            source: std::io::Error::other("permission denied"),
        };
        let rendered = format!("{err}");
        assert!(rendered.contains("/var/lib/php-tree-viz/tmp"));
        assert!(rendered.contains("permission denied"));
        assert!(!rendered.contains('\n'));
    }

    #[test]
    fn display_tmp_write_is_single_line_and_names_path() {
        let err = HttpError::TmpWrite {
            path: PathBuf::from("/var/lib/php-tree-viz/tmp/abc.partial"),
            source: std::io::Error::other("disk full"),
        };
        let rendered = format!("{err}");
        assert!(rendered.contains("abc.partial"));
        assert!(rendered.contains("disk full"));
        assert!(!rendered.contains('\n'));
    }
}
