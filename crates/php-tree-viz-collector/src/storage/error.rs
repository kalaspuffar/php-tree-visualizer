//! Error type for the storage layer.
//!
//! Same single-line `Display` convention as `HttpError` and
//! `ConfigError`: every variant renders as one line so a stderr
//! emit doesn't break the `decoder:` / `storage:` log format the
//! operator greps against.

use std::path::PathBuf;

#[derive(Debug)]
pub enum StorageError {
    /// `rusqlite::Connection::open` failed (path inaccessible,
    /// not a regular file, permissions, etc.).
    Open {
        path: PathBuf,
        source: rusqlite::Error,
    },
    /// Applying the embedded schema to a fresh DB failed.
    SchemaApply {
        path: PathBuf,
        source: rusqlite::Error,
    },
    /// An existing DB carries a `PRAGMA user_version` we don't
    /// recognise. Per `SPECIFICATION.md` §8.2 this means the
    /// operator deployed a binary older or newer than the file's
    /// schema; refuse to touch it rather than corrupt data.
    UnknownSchemaVersion { path: PathBuf, got: u32 },
    /// A SQL statement during `record_batch` failed (transaction
    /// open, prepared-statement execute, commit, etc.). The
    /// `context` string names the operation so the operator can
    /// grep for it.
    Query {
        context: &'static str,
        source: rusqlite::Error,
    },
    /// A filesystem operation outside the SQLite layer failed —
    /// `metadata`, `read_dir`, `remove_file`, `remove_dir_all`.
    /// Used by the retention sweeper when stat-ing or unlinking
    /// the per-trace files. `NotFound` is *not* surfaced as this
    /// variant; the sweeper treats missing files as success.
    Io {
        context: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open { path, source } => {
                write!(
                    f,
                    "could not open sqlite file {}: {}",
                    path.display(),
                    source
                )
            }
            Self::SchemaApply { path, source } => {
                write!(
                    f,
                    "could not apply schema to {}: {}",
                    path.display(),
                    source
                )
            }
            Self::UnknownSchemaVersion { path, got } => {
                write!(
                    f,
                    "unknown PRAGMA user_version {got} on {} (expected 0 or 1)",
                    path.display()
                )
            }
            Self::Query { context, source } => {
                write!(f, "sqlite query failed ({context}): {source}")
            }
            Self::Io {
                context,
                path,
                source,
            } => {
                write!(
                    f,
                    "filesystem operation failed ({context}) at {}: {source}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Open { source, .. }
            | Self::SchemaApply { source, .. }
            | Self::Query { source, .. } => Some(source),
            Self::Io { source, .. } => Some(source),
            Self::UnknownSchemaVersion { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_open_is_single_line() {
        let err = StorageError::Open {
            path: PathBuf::from("/var/lib/x/index.sqlite"),
            source: rusqlite::Error::InvalidPath(PathBuf::from("/x")),
        };
        let rendered = format!("{err}");
        assert!(rendered.contains("/var/lib/x/index.sqlite"));
        assert!(!rendered.contains('\n'));
    }

    #[test]
    fn display_unknown_schema_version_names_value() {
        let err = StorageError::UnknownSchemaVersion {
            path: PathBuf::from("/x/index.sqlite"),
            got: 99,
        };
        let rendered = format!("{err}");
        assert!(rendered.contains("99"));
        assert!(rendered.contains("/x/index.sqlite"));
        assert!(rendered.contains("expected 0 or 1"));
    }

    #[test]
    fn source_chain_is_set_where_applicable() {
        let q = StorageError::Query {
            context: "upsert_trace",
            source: rusqlite::Error::QueryReturnedNoRows,
        };
        assert!(std::error::Error::source(&q).is_some());

        let v = StorageError::UnknownSchemaVersion {
            path: PathBuf::from("/x"),
            got: 2,
        };
        assert!(std::error::Error::source(&v).is_none());
    }
}
