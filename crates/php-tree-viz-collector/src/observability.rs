//! Observability surface: tracing-subscriber install, disk-usage gauge.
//!
//! Owned by the `collector-observability` capability. This module is
//! the single sink for the collector's log output (replacing all
//! historical `println!` / `eprintln!` calls in `src/`) and the home
//! of the periodic disk-usage gauge task.
//!
//! Subscriber install lives in `subscriber.rs`; the gauge task lives
//! in `disk_usage.rs`. Both are surfaced here so the rest of the
//! crate has one import root.

mod disk_usage;
mod subscriber;

pub use disk_usage::{disk_usage_loop, measure_data_dir};
pub use subscriber::{install_subscriber, InstallError};
