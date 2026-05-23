//! The collector binary.
//!
//! Load the TOML config, then bind an HTTP listener and serve until
//! SIGTERM / SIGINT. Exit codes:
//!
//! - `0` — clean shutdown.
//! - `1` — reserved for the Rust panic default.
//! - `2` — configuration problem (bad CLI args, file unreadable,
//!   parse error, validation failure).
//! - `3` — HTTP / bind error.

mod config;
mod finalize;
mod http;
mod storage;
mod tracekey;
mod wire;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use crate::config::{load_from_path, Config, ConfigError};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let path = match parse_args(&args) {
        Ok(p) => p,
        Err(err) => {
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
    print_startup_summary(&path, &cfg);

    // Build a multi-thread runtime; bring axum up; serve until signal.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(err) => {
            eprintln!("http error: could not start tokio runtime: {err}");
            return ExitCode::from(3);
        }
    };

    match runtime.block_on(http::run(Arc::new(cfg))) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("http error: {err}");
            ExitCode::from(3)
        }
    }
}

fn print_startup_summary(path: &std::path::Path, cfg: &Config) {
    // Same redacted summary as before — secrets pass through their
    // Display impls (`***`). This is the operator's startup banner.
    println!(
        "loaded config from {}: bind={} data_dir={} retention={}d \
         queue_capacity={} max_body_bytes={} log={}/{} token={} salt={}",
        path.display(),
        cfg.server.bind,
        cfg.storage.data_dir.display(),
        cfg.storage.retention_days,
        cfg.server.queue_capacity,
        cfg.server.max_body_bytes,
        cfg.log.level,
        cfg.log.format,
        cfg.auth.token,
        cfg.auth.session_salt,
    );
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
