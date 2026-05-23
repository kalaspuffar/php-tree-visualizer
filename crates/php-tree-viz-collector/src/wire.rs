//! MessagePack wire types for the ingest path.
//!
//! Today only the `meta` block is decoded — enough for the ingest
//! handler to determine the `TraceKey` and to reject non-v1 bodies.
//! Subsequent changes will add `DictEntry`, `Call`, and a full
//! `Batch` deserializer alongside.
//!
//! Field shapes mirror `handover/WIRE_FORMAT.md` one-to-one — same
//! names, same types, same order. The wire format is the
//! authoritative source (per `SPECIFICATION.md` §12.3).

use serde::Deserialize;

/// The eight fields of the `meta` map. Per the wire format's
/// forward-compatibility rule (INV-6), unknown extra keys are
/// silently ignored — we do *not* set `deny_unknown_fields`.
///
/// `sapi`, `uri_or_script`, and `dropped_records` are decoded but
/// not yet consumed by this slice; they will be read by the
/// storage / index-DB capability when it lands. We carry them now
/// so the wire-format mapping stays one-to-one with `WIRE_FORMAT.md`
/// and so a future change adds zero parser churn.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct Meta {
    pub schema_version: u32,
    pub trace_id: String,
    pub host: String,
    pub pid: u64,
    pub start_time: i64,
    pub sapi: String,
    pub uri_or_script: String,
    pub dropped_records: u64,
}

/// Errors produced by [`parse_meta`]. Each variant's `Display` is a
/// single line so it can be surfaced verbatim in the `400`
/// response's `detail` field without breaking the
/// one-line-per-error convention used elsewhere in the crate.
#[derive(Debug)]
pub enum WireError {
    /// `rmp_serde` could not decode the body, or could not find the
    /// `meta` field, or `meta` was missing required keys.
    Parse(rmp_serde::decode::Error),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Collapse any newlines in the underlying error so we
            // remain one line of output.
            Self::Parse(e) => {
                let collapsed: String = e
                    .to_string()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                f.write_str(&collapsed)
            }
        }
    }
}

impl std::error::Error for WireError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Parse(e) => Some(e),
        }
    }
}

/// Parse just the `meta` block out of a MessagePack batch body.
///
/// Uses a private wrapper that names only the `meta` field; serde
/// skips the (potentially large) `dict` and `calls` arrays. This is
/// not free — the deserializer still walks those structures — but
/// it requires no manual byte-level seeking and is fast enough for
/// the documented batch sizes (§7.2).
pub fn parse_meta(body: &[u8]) -> Result<Meta, WireError> {
    #[derive(Deserialize)]
    struct BatchHeader {
        meta: Meta,
    }
    let header: BatchHeader = rmp_serde::from_slice(body).map_err(WireError::Parse)?;
    Ok(header.meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;
    use std::collections::BTreeMap;

    // A real captured batch (gitignored `handover/` was the source);
    // the file lives under `tests/fixtures/` so it is tracked in git
    // and reachable in CI.
    const FIXTURE_FLAT_CALLS_1: &[u8] =
        include_bytes!("../tests/fixtures/flat_calls/batch-0001.msgpack");

    /// Test-only Serialize-friendly Meta. Mirrors the real Meta
    /// field-by-field so `rmp_serde::to_vec_named` produces a body
    /// our parser accepts.
    #[derive(Serialize, Clone)]
    struct TestMeta {
        schema_version: u32,
        trace_id: String,
        host: String,
        pid: u64,
        start_time: i64,
        sapi: String,
        uri_or_script: String,
        dropped_records: u64,
    }

    impl TestMeta {
        fn valid() -> Self {
            Self {
                schema_version: 1,
                trace_id: "00000000-0000-0000-0000-000000000000".into(),
                host: "test-host".into(),
                pid: 12345,
                start_time: 1_700_000_000_000_000_000,
                sapi: "cli".into(),
                uri_or_script: "/tmp/x.php".into(),
                dropped_records: 0,
            }
        }
    }

    /// Build a full MessagePack batch (meta + dict + calls) with
    /// the given meta. `dict` and `calls` are empty arrays.
    fn build_batch_with(meta: &TestMeta) -> Vec<u8> {
        #[derive(Serialize)]
        struct TestBatch<'a> {
            meta: &'a TestMeta,
            dict: Vec<()>,
            calls: Vec<()>,
        }
        rmp_serde::to_vec_named(&TestBatch {
            meta,
            dict: vec![],
            calls: vec![],
        })
        .unwrap()
    }

    #[test]
    fn parses_real_captured_fixture() {
        let meta = parse_meta(FIXTURE_FLAT_CALLS_1).expect("fixture must parse");
        assert_eq!(meta.schema_version, 1);
        assert_eq!(meta.trace_id, "00000000-0000-0000-0000-000000000000");
        assert!(!meta.host.is_empty(), "host must be non-empty");
        assert!(meta.pid > 0);
        assert!(meta.start_time > 0);
    }

    #[test]
    fn parses_synthetic_batch_with_named_map() {
        let body = build_batch_with(&TestMeta::valid());
        let meta = parse_meta(&body).expect("synthetic batch must parse");
        assert_eq!(meta.schema_version, 1);
        assert_eq!(meta.host, "test-host");
        assert_eq!(meta.pid, 12345);
    }

    #[test]
    fn non_msgpack_body_is_rejected() {
        let body = b"hello, world";
        let err = parse_meta(body).expect_err("garbage must fail");
        // Display must be a single line.
        let rendered = format!("{err}");
        assert!(!rendered.is_empty());
        assert!(!rendered.contains('\n'), "multi-line: {rendered}");
    }

    #[test]
    fn top_level_scalar_is_rejected() {
        // Encode a single integer as the entire body.
        let body = rmp_serde::to_vec(&42u32).unwrap();
        assert!(parse_meta(&body).is_err());
    }

    #[test]
    fn body_missing_meta_field_is_rejected() {
        // A map containing only `dict` and `calls`, no `meta`.
        #[derive(Serialize)]
        struct NoMeta {
            dict: Vec<()>,
            calls: Vec<()>,
        }
        let body = rmp_serde::to_vec_named(&NoMeta {
            dict: vec![],
            calls: vec![],
        })
        .unwrap();
        assert!(parse_meta(&body).is_err());
    }

    #[test]
    fn meta_missing_required_field_is_rejected() {
        // Build a meta missing `schema_version`.
        let mut map = BTreeMap::new();
        map.insert(
            "trace_id",
            rmp_serde::to_vec(&"00000000-0000-0000-0000-000000000000".to_string()).unwrap(),
        );
        // We can't easily build a partial struct via serde directly,
        // but we can write the wire-format map manually:
        #[derive(Serialize)]
        struct PartialMeta {
            trace_id: String,
            host: String,
            pid: u64,
            start_time: i64,
            sapi: String,
            uri_or_script: String,
            dropped_records: u64,
        }
        #[derive(Serialize)]
        struct B {
            meta: PartialMeta,
            dict: Vec<()>,
            calls: Vec<()>,
        }
        let body = rmp_serde::to_vec_named(&B {
            meta: PartialMeta {
                trace_id: "00000000-0000-0000-0000-000000000000".into(),
                host: "h".into(),
                pid: 1,
                start_time: 1,
                sapi: "cli".into(),
                uri_or_script: "x".into(),
                dropped_records: 0,
            },
            dict: vec![],
            calls: vec![],
        })
        .unwrap();
        assert!(parse_meta(&body).is_err());
        let _ = map; // silence unused warning from the BTreeMap stub
    }

    #[test]
    fn forward_compatible_unknown_fields_in_meta_are_ignored() {
        // The wire promises future v1-additive keys; we must not reject them.
        #[derive(Serialize)]
        struct MetaPlusExtra {
            schema_version: u32,
            trace_id: String,
            host: String,
            pid: u64,
            start_time: i64,
            sapi: String,
            uri_or_script: String,
            dropped_records: u64,
            future_field: String,
        }
        #[derive(Serialize)]
        struct B {
            meta: MetaPlusExtra,
            dict: Vec<()>,
            calls: Vec<()>,
        }
        let body = rmp_serde::to_vec_named(&B {
            meta: MetaPlusExtra {
                schema_version: 1,
                trace_id: "00000000-0000-0000-0000-000000000000".into(),
                host: "h".into(),
                pid: 1,
                start_time: 1,
                sapi: "cli".into(),
                uri_or_script: "x".into(),
                dropped_records: 0,
                future_field: "ignored".into(),
            },
            dict: vec![],
            calls: vec![],
        })
        .unwrap();
        let meta = parse_meta(&body).expect("unknown fields must be silently skipped");
        assert_eq!(meta.schema_version, 1);
    }
}
