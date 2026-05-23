//! The collector binary.
//!
//! Today this is a thin wrapper around the config loader: parse the
//! `--config <path>` CLI flag, load and validate the TOML file at that
//! path, print a single redacted summary line, and exit. Subsequent
//! OpenSpec changes will replace the `println!` with the HTTP server,
//! decoder, storage, finalize, and retention modules listed in
//! `SPECIFICATION.md` §3.1.

mod config;

use std::path::PathBuf;
use std::process::ExitCode;

use crate::config::{load_from_path, ConfigError};

fn main() -> ExitCode {
    match run(std::env::args().collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // Single-line stderr; stdout stays untouched.
            eprintln!("config error: {err}");
            // Exit 2 = configuration problem. 1 is reserved for the
            // Rust panic default so the operator can distinguish
            // "config is wrong" from "the program crashed."
            ExitCode::from(2)
        }
    }
}

fn run(args: Vec<String>) -> Result<(), ConfigError> {
    let path = parse_args(&args)?;
    let cfg = load_from_path(&path)?;
    // The summary line is the only thing on stdout. Secrets are
    // redacted by their Display impls; INV-2 is preserved.
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
    Ok(())
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
