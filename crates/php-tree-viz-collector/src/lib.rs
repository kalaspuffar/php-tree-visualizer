//! Library crate for the collector.
//!
//! The binary (`src/main.rs`) is a thin entrypoint that wires this
//! library's modules together. Tests and benchmarks reach in here
//! directly via `php_tree_viz_collector::<module>`.

pub mod config;
pub mod finalize;
pub mod http;
pub mod observability;
pub mod retention;
pub mod storage;
pub mod tracekey;
pub mod wire;
