//! The collector binary.
//!
//! Load the TOML config, install the tracing subscriber, then bind an
//! HTTP listener and serve until SIGTERM / SIGINT. Exit codes:
//!
//! - `0` — clean shutdown.
//! - `1` — reserved for the Rust panic default.
//! - `2` — configuration problem (bad CLI args, file unreadable,
//!   parse error, validation failure).
//! - `3` — HTTP / bind / storage / subscriber-install error.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use php_tree_viz_collector::config::{load_from_path, ConfigError};
use php_tree_viz_collector::{http, observability};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let path = match parse_args(&args) {
        Ok(p) => p,
        Err(err) => {
            // Pre-subscriber: write straight to stderr. The
            // subscriber isn't installed yet, so we can't route
            // through tracing.
            eprintln!("config error: {err}");
            return ExitCode::from(2);
        }
    };
    let cfg = match load_from_path(&path) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("config error: {err}");
            return ExitCode::from(2);
        }
    };

    // Install the subscriber before anything that could log.
    // Subscriber-install failure exits status 3 with a single stderr
    // line — same family as bind / storage failures.
    if let Err(err) = observability::install_subscriber(&cfg.log) {
        eprintln!("observability error: {err}");
        return ExitCode::from(3);
    }

    tracing::info!(
        target: "config",
        path = %path.display(),
        bind = %cfg.server.bind,
        data_dir = %cfg.storage.data_dir.display(),
        retention_days = cfg.storage.retention_days,
        queue_capacity = cfg.server.queue_capacity,
        max_body_bytes = cfg.server.max_body_bytes,
        log_level = %cfg.log.level,
        log_format = %cfg.log.format,
        "configuration loaded"
    );

    // Build a multi-thread runtime; bring axum up; serve until signal.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(err) => {
            tracing::error!(reason = %err, "could not start tokio runtime");
            return ExitCode::from(3);
        }
    };

    match runtime.block_on(http::run(Arc::new(cfg))) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(reason = %err, "http server failed");
            ExitCode::from(3)
        }
    }
}

/// Hand-rolled CLI parser. Recognises exactly one flag: `--config
/// <path>`. Any other input — missing flag, missing value, repeated
/// flag, or unknown argument — produces a `ConfigError::Cli`.
fn parse_args(args: &[String]) -> Result<PathBuf, ConfigError> {
    let mut iter = args.iter();
    // Skip the program name (`args[0]`); tolerate it being absent.
    let _program = iter.next();
    let mut path: Option<PathBuf> = None;
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" => {
                let value = iter.next().ok_or_else(|| ConfigError::Cli {
                    reason: "--config requires a path argument".into(),
                })?;
                if path.is_some() {
                    return Err(ConfigError::Cli {
                        reason: "--config may only be given once".into(),
                    });
                }
                path = Some(PathBuf::from(value));
            }
            other => {
                return Err(ConfigError::Cli {
                    reason: format!(
                        "unknown argument: {other} (only --config <path> is recognised)"
                    ),
                });
            }
        }
    }
    path.ok_or_else(|| ConfigError::Cli {
        reason: "missing required --config <path> flag".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn missing_config_flag_produces_a_clear_error() {
        let err = parse_args(&args(&["bin"])).expect_err("must fail");
        let msg = format!("{err}");
        assert!(msg.contains("--config"));
    }

    #[test]
    fn config_with_path_succeeds() {
        let p = parse_args(&args(&["bin", "--config", "/x/y.toml"])).expect("must succeed");
        assert_eq!(p, PathBuf::from("/x/y.toml"));
    }

    #[test]
    fn config_without_value_fails() {
        let err = parse_args(&args(&["bin", "--config"])).expect_err("must fail");
        assert!(format!("{err}").contains("requires a path"));
    }

    #[test]
    fn unknown_flag_fails_and_names_the_flag() {
        let err = parse_args(&args(&["bin", "--bogus"])).expect_err("must fail");
        assert!(format!("{err}").contains("--bogus"));
    }

    #[test]
    fn repeated_config_flag_fails() {
        let err =
            parse_args(&args(&["bin", "--config", "/a", "--config", "/b"])).expect_err("must fail");
        assert!(format!("{err}").to_lowercase().contains("once"));
    }

    #[test]
    fn unknown_value_after_config_is_treated_as_unknown_flag() {
        // `--config /a extra` — `extra` doesn't start with `--`, but
        // we still reject it because we don't recognise positional
        // arguments. This is a deliberate "no positional args" stance.
        let err = parse_args(&args(&["bin", "--config", "/a", "extra"])).expect_err("must fail");
        assert!(format!("{err}").contains("extra"));
    }
}
