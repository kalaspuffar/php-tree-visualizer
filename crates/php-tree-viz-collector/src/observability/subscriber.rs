//! Single-source-of-truth tracing subscriber install.
//!
//! Called once from `main.rs` after the configuration has been loaded
//! and validated. Picks the layer (`fmt` for `text`, `fmt::json` for
//! `json`) by reading `config.log.format`, picks the filter level by
//! reading `RUST_LOG` (when set and parseable) or
//! `config.log.level` (the fallback), composes them into a
//! `Registry`, and calls `.init()`.
//!
//! Subscriber install is global and at-most-once per process. A
//! second install attempt returns
//! `InstallError::SubscriberAlreadyInstalled`. That should never
//! happen at runtime (a single `main` calls `install_subscriber`
//! once); it does happen in tests that drive the subscriber
//! themselves, where they bypass this function.

use std::env;

use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::Registry;

use crate::config::Log;

const RUST_LOG: &str = "RUST_LOG";

/// Outcome of attempting to install the global subscriber. The
/// caller (today: `main.rs`) treats both variants as "exit 3 with a
/// single stderr line" since they happen before the subscriber is
/// available to route the message.
#[derive(Debug)]
pub enum InstallError {
    /// `tracing::subscriber::set_global_default` rejected the
    /// registration. The only realistic cause is a duplicate
    /// install in the same process.
    SubscriberAlreadyInstalled,
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SubscriberAlreadyInstalled => f.write_str(
                "could not install the tracing subscriber: another subscriber is already global",
            ),
        }
    }
}

impl std::error::Error for InstallError {}

/// Install the process-wide tracing subscriber driven by
/// `config.log`. Honour `RUST_LOG` when it parses; warn-and-fall-back
/// when it doesn't; never exit.
pub fn install_subscriber(log: &Log) -> Result<(), InstallError> {
    let env_value = env::var(RUST_LOG).ok();
    let (filter, outcome) = build_filter(env_value.as_deref(), &log.level);
    match log.format.as_str() {
        "json" => install_json(filter)?,
        // The validator restricts log.format to "json" | "text", so
        // anything other than "json" is "text". Default behaviour
        // matches `Log::default().format = "json"`, but the validator
        // is the authoritative gate — see `collector-config`.
        _ => install_text(filter)?,
    }
    log_filter_outcome(outcome, &log.level);
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum FilterOutcome {
    /// `RUST_LOG` was unset; we used `config.log.level`.
    UsedConfigLevel,
    /// `RUST_LOG` was set and parsed; we used that.
    UsedRustLog { value: String },
    /// `RUST_LOG` was set but did not parse; we fell back to
    /// `config.log.level` and need to warn after install.
    InvalidRustLogFellBack { value: String },
}

/// Pure function: pick a filter given an optional `RUST_LOG`-style
/// string and the fallback config level. Testable without touching
/// process env state (which is shared across parallel tests).
fn build_filter(env_value: Option<&str>, config_level: &str) -> (EnvFilter, FilterOutcome) {
    match env_value {
        Some(raw) => match EnvFilter::try_new(raw) {
            Ok(filter) => (
                filter,
                FilterOutcome::UsedRustLog {
                    value: raw.to_owned(),
                },
            ),
            Err(_) => (
                EnvFilter::new(config_level),
                FilterOutcome::InvalidRustLogFellBack {
                    value: raw.to_owned(),
                },
            ),
        },
        None => (EnvFilter::new(config_level), FilterOutcome::UsedConfigLevel),
    }
}

fn install_text(filter: EnvFilter) -> Result<(), InstallError> {
    // Disable ANSI so journald (and CI captures) stay readable. The
    // operator's interactive `journalctl --output cat` view does not
    // benefit from escape sequences.
    let layer = fmt::layer().with_target(true).with_ansi(false);
    Registry::default()
        .with(filter)
        .with(layer)
        .try_init()
        .map_err(|_| InstallError::SubscriberAlreadyInstalled)
}

fn install_json(filter: EnvFilter) -> Result<(), InstallError> {
    let layer = fmt::layer()
        .json()
        .with_current_span(false)
        .flatten_event(true);
    Registry::default()
        .with(filter)
        .with(layer)
        .try_init()
        .map_err(|_| InstallError::SubscriberAlreadyInstalled)
}

fn log_filter_outcome(outcome: FilterOutcome, config_level: &str) {
    match outcome {
        FilterOutcome::UsedConfigLevel => {}
        FilterOutcome::UsedRustLog { value } => {
            tracing::info!(rust_log = %value, "RUST_LOG overriding config.log.level");
        }
        FilterOutcome::InvalidRustLogFellBack { value } => {
            tracing::warn!(
                rust_log = %value,
                fallback_level = %config_level,
                "invalid RUST_LOG; falling back to config.log.level"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_filter_falls_back_when_rust_log_unset() {
        let (_filter, outcome) = build_filter(None, "warn");
        assert_eq!(outcome, FilterOutcome::UsedConfigLevel);
    }

    #[test]
    fn build_filter_uses_rust_log_when_valid() {
        let (_filter, outcome) = build_filter(Some("debug"), "info");
        assert_eq!(
            outcome,
            FilterOutcome::UsedRustLog {
                value: "debug".to_owned()
            }
        );
    }

    #[test]
    fn build_filter_falls_back_when_rust_log_invalid() {
        // `target=invalidlevel` is one shape `EnvFilter::try_new`
        // rejects: the level position is required to be a
        // LevelFilter literal. We don't pin the exact rejection
        // grammar of tracing-subscriber — the spec contract is
        // "if EnvFilter returns Err, we fall back" — so the assert
        // guards the fixture against a future relaxation.
        let bad = "target=invalidlevel";
        assert!(
            EnvFilter::try_new(bad).is_err(),
            "test fixture must be an EnvFilter parse error; \
             tracing-subscriber may have relaxed this — update the fixture"
        );

        let (_filter, outcome) = build_filter(Some(bad), "info");
        assert_eq!(
            outcome,
            FilterOutcome::InvalidRustLogFellBack {
                value: bad.to_owned()
            }
        );
    }

    #[test]
    fn install_error_displays_a_single_line() {
        let err = InstallError::SubscriberAlreadyInstalled;
        let rendered = format!("{err}");
        assert!(!rendered.contains('\n'));
        assert!(rendered.contains("subscriber"));
    }
}
