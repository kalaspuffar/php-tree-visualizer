//! Shared test helpers. Owned by the test binaries, not the crate.
//!
//! Each integration-test binary that uses these mounts them with
//! `mod support;` — Cargo compiles `tests/support/mod.rs` into each
//! consuming binary separately, so a `dead_code` warning is normal
//! when a particular binary only uses a subset.

#![allow(dead_code)]

pub mod batch;
pub mod harness;
pub mod log_capture;
