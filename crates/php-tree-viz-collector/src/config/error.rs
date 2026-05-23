//! The error type produced by the config loader.
//!
//! Each variant carries enough context that `Display` produces a single,
//! operator-readable line — the spec's promise that a config failure is
//! one line on stderr (and stdout stays empty).

use std::path::{Path, PathBuf};

/// The single error type for everything the loader can refuse: I/O,
/// TOML parse, validation, and CLI argument problems.
#[derive(Debug)]
pub enum ConfigError {
    /// Reading the file failed (missing, unreadable, not UTF-8, etc.).
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The file was read but did not parse as TOML, or carried fields
    /// the loader does not recognise, or omitted a required field.
    /// Surface for the `deny_unknown_fields` rejection too.
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    /// A field parsed correctly but its value is outside the rules
    /// (token too short, bind not a `SocketAddr`, etc.).
    BadValue { field: &'static str, reason: String },
    /// The CLI arguments were malformed (missing `--config`, repeated
    /// flag, unknown flag, missing value).
    Cli { reason: String },
}

impl ConfigError {
    /// Helper for validation: construct a `BadValue` with a static
    /// field name and a free-form reason.
    pub(crate) fn bad_value(field: &'static str, reason: impl Into<String>) -> Self {
        Self::BadValue {
            field,
            reason: reason.into(),
        }
    }

    /// Helper used by [`crate::config::load_from_path`] to attach the
    /// config path to an I/O failure.
    pub(crate) fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }

    /// Helper used by [`crate::config::load_from_path`] to attach the
    /// config path to a TOML parse failure.
    pub(crate) fn parse(path: &Path, source: toml::de::Error) -> Self {
        Self::Parse {
            path: path.to_path_buf(),
            source,
        }
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(
                    f,
                    "could not read config file {}: {}",
                    path.display(),
                    source
                )
            }
            Self::Parse { path, source } => {
                // `toml::de::Error`'s `Display` produces a multi-line
                // snippet (`5 | foo = "bar"\n   | ^^^`). The spec says
                // one line on stderr — collapse all whitespace runs.
                let collapsed = collapse_whitespace(&source.to_string());
                write!(
                    f,
                    "could not parse config file {}: {}",
                    path.display(),
                    collapsed
                )
            }
            Self::BadValue { field, reason } => {
                write!(f, "invalid value for {field}: {reason}")
            }
            Self::Cli { reason } => f.write_str(reason),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            Self::BadValue { .. } | Self::Cli { .. } => None,
        }
    }
}

fn collapse_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_io_includes_path_and_reason() {
        let err = ConfigError::io(
            Path::new("/etc/missing.toml"),
            std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
        );
        let rendered = format!("{err}");
        assert!(rendered.contains("/etc/missing.toml"));
        assert!(rendered.contains("no such file"));
        assert!(!rendered.contains('\n'));
    }

    #[test]
    fn display_bad_value_includes_field_and_reason() {
        let err = ConfigError::bad_value("auth.token", "must be at least 32 characters");
        let rendered = format!("{err}");
        assert!(rendered.contains("auth.token"));
        assert!(rendered.contains("must be at least 32 characters"));
        assert!(!rendered.contains('\n'));
    }

    #[test]
    fn display_cli_is_the_reason_string() {
        let err = ConfigError::Cli {
            reason: "missing required --config <path> flag".into(),
        };
        assert_eq!(format!("{err}"), "missing required --config <path> flag");
    }

    #[test]
    fn collapse_whitespace_flattens_newlines() {
        assert_eq!(
            collapse_whitespace("first line\n  second  line\n\nthird"),
            "first line second line third"
        );
    }

    #[test]
    fn display_parse_is_single_line() {
        // Build a toml::de::Error by parsing something obviously bad.
        let err = match toml::from_str::<toml::Value>("not = valid = toml") {
            Ok(_) => panic!("expected parse failure"),
            Err(e) => e,
        };
        let wrapped = ConfigError::parse(Path::new("/x/y.toml"), err);
        let rendered = format!("{wrapped}");
        assert!(!rendered.contains('\n'), "multi-line: {rendered}");
        assert!(rendered.contains("/x/y.toml"));
    }

    #[test]
    fn source_chain_is_set_for_io_and_parse() {
        let io_err = ConfigError::io(
            Path::new("/x"),
            std::io::Error::new(std::io::ErrorKind::NotFound, "nope"),
        );
        assert!(std::error::Error::source(&io_err).is_some());

        let parse_err = ConfigError::parse(
            Path::new("/x"),
            toml::from_str::<toml::Value>("===").unwrap_err(),
        );
        assert!(std::error::Error::source(&parse_err).is_some());

        let bad = ConfigError::bad_value("server.bind", "nope");
        assert!(std::error::Error::source(&bad).is_none());
    }
}
