//! MessagePack wire types for the ingest path.
//!
//! Two parsers live here:
//!
//! - [`parse_meta`] — used on the request path by the HTTP handler.
//!   Cheap: only the `meta` block's eight fields are materialised;
//!   `dict` and `calls` are walked-and-discarded by serde.
//! - [`parse_batch`] — used by the decoder task after a successful
//!   commit. Full: every `DictEntry` and `Call` is allocated as a
//!   Rust value. Roughly 100× the cost of `parse_meta`; runs off
//!   the hot request path. See design D-1 of the `wire-decoder`
//!   change for the cost/scope rationale.
//!
//! Field shapes mirror `handover/WIRE_FORMAT.md` one-to-one — same
//! names, same types, same order. The wire format is the
//! authoritative source (per `SPECIFICATION.md` §12.3).
//!
//! Forward compatibility (INV-6): no struct uses
//! `deny_unknown_fields`. Unknown extra keys at any nesting level
//! are silently ignored, matching the wire promise that v1 may add
//! keys without bumping the schema version.

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

/// One "first-sight" function entry inside `dict`. Each function
/// referenced by a `Call.fn_id` SHALL appear in the `dict` of the
/// first batch in its trace where it's referenced; subsequent
/// batches in the same trace omit it. See `handover/WIRE_FORMAT.md`.
///
/// Like `Meta`, fields are decoded but not yet consumed by this
/// slice; the storage / aggregation slices will read them.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct DictEntry {
    /// Per-trace function id. Monotonic from 1. Referenced by
    /// every `Call.fn_id`.
    pub fn_id: u32,
    /// Fully-qualified name. Conventions per the wire spec:
    /// bare for user functions and internals, `Class::method` for
    /// methods, `closure:<file>:<line>` for closures.
    pub fqn: String,
    /// Absolute path to the declaring file. Empty (`""`) for
    /// internal functions.
    pub file: String,
    /// Declaring line. `0` for internal functions.
    pub line: u32,
    /// Function kind: `0` = function, `1` = method, `2` =
    /// closure, `3` = internal. v1 freezes this mapping. Decoded
    /// as `u8` for byte-economy; validation (the `0..=3` range)
    /// is the aggregation slice's job.
    pub kind: u8,
}

/// One per-call record inside `calls`. Each record SHALL be unique
/// per `(trace_id, call_id)`. The Recorder pushes records in the
/// order calls exited (PHP `return`); array position within a
/// batch is meaningful, but reconstruction uses
/// `(call_id, parent)`. See `handover/WIRE_FORMAT.md`.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct Call {
    /// Unique per `(trace_id, call_id)`. Monotonic from 1.
    pub call_id: u32,
    /// The `call_id` of the calling frame. `0` means "no parent"
    /// (top-level call).
    pub parent: u32,
    /// Reference to the `DictEntry.fn_id`. The wire-level key is
    /// `fn` (shortened from `fn_id` for byte economy); `fn` is a
    /// Rust reserved word so the field is `fn_id` and serde-renamed.
    #[serde(rename = "fn")]
    pub fn_id: u32,
    /// Call-stack depth at call time. Script body = `0`; its
    /// direct callees = `1`.
    pub depth: u32,
    /// `CLOCK_MONOTONIC` ns at function entry. Compare to `t_out`
    /// for duration; do NOT compare to `Meta.start_time` (different
    /// clock domain — INV-3).
    pub t_in: i64,
    /// `CLOCK_MONOTONIC` ns at function exit.
    pub t_out: i64,
    /// User-mode CPU ns consumed by this call. May be `0` for
    /// sub-µs calls or when `cpu_snapshot_mode = off`.
    pub cpu_u: i64,
    /// Kernel-mode CPU ns consumed. Same caveats as `cpu_u`.
    pub cpu_s: i64,
    /// `zend_memory_usage(true)` at function entry.
    pub mem_in: i64,
    /// `zend_memory_usage(true)` at function exit.
    pub mem_out: i64,
    /// `true` when the function exited via an unhandled exception
    /// unwinding past the call boundary; `false` on normal return.
    pub abnormal_exit: bool,
}

/// The full v1 batch: top-level map with three fields. Decoded by
/// [`parse_batch`] off the hot path (in the decoder task).
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct Batch {
    pub meta: Meta,
    pub dict: Vec<DictEntry>,
    pub calls: Vec<Call>,
}

/// Parse just the `meta` block out of a MessagePack batch body.
///
/// Uses a private wrapper that names only the `meta` field; serde
/// skips the (potentially large) `dict` and `calls` arrays. This is
/// not free — the deserializer still walks those structures — but
/// it requires no manual byte-level seeking and is fast enough for
/// the documented batch sizes (§7.2).
///
/// Compare to [`parse_batch`] for the full-decode path.
pub fn parse_meta(body: &[u8]) -> Result<Meta, WireError> {
    #[derive(Deserialize)]
    struct BatchHeader {
        meta: Meta,
    }
    let header: BatchHeader = rmp_serde::from_slice(body).map_err(WireError::Parse)?;
    Ok(header.meta)
}

/// Parse the full v1 batch: meta, dict, and every call.
///
/// Used by the decoder task after a successful commit. Materialises
/// every `DictEntry` and `Call` as a Rust value — ~10 ms / ~10K
/// allocations for a 10K-call batch. Roughly 100× the cost of
/// [`parse_meta`]. Stays off the request hot path.
pub fn parse_batch(body: &[u8]) -> Result<Batch, WireError> {
    rmp_serde::from_slice(body).map_err(WireError::Parse)
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

    // ---- Helpers for parse_batch tests ----

    /// Test-only Serialize mirrors of the wire types. They must
    /// match `DictEntry` / `Call` field-by-field on the wire (same
    /// names, same types, same renames). Anything written via
    /// `rmp_serde::to_vec_named` is then a valid input for
    /// `parse_batch`.
    #[derive(Serialize, Clone)]
    struct TestDictEntry {
        fn_id: u32,
        fqn: String,
        file: String,
        line: u32,
        kind: u8,
    }

    #[derive(Serialize, Clone)]
    struct TestCall {
        call_id: u32,
        parent: u32,
        // Wire key is `fn`; mirror that on the Serialize side too
        // so the bytes match what `parse_batch` expects.
        #[serde(rename = "fn")]
        fn_id: u32,
        depth: u32,
        t_in: i64,
        t_out: i64,
        cpu_u: i64,
        cpu_s: i64,
        mem_in: i64,
        mem_out: i64,
        abnormal_exit: bool,
    }

    impl TestDictEntry {
        fn sample(fn_id: u32) -> Self {
            Self {
                fn_id,
                fqn: format!("ns\\func_{fn_id}"),
                file: "/tmp/x.php".into(),
                line: 1,
                kind: 0,
            }
        }
    }

    impl TestCall {
        fn sample(call_id: u32, parent: u32, fn_id: u32) -> Self {
            Self {
                call_id,
                parent,
                fn_id,
                depth: 0,
                t_in: 0,
                t_out: 1,
                cpu_u: 0,
                cpu_s: 0,
                mem_in: 0,
                mem_out: 0,
                abnormal_exit: false,
            }
        }
    }

    fn build_full_batch(
        meta: &TestMeta,
        dict: Vec<TestDictEntry>,
        calls: Vec<TestCall>,
    ) -> Vec<u8> {
        #[derive(Serialize)]
        struct TestBatch<'a> {
            meta: &'a TestMeta,
            dict: Vec<TestDictEntry>,
            calls: Vec<TestCall>,
        }
        rmp_serde::to_vec_named(&TestBatch { meta, dict, calls }).unwrap()
    }

    #[test]
    fn parse_batch_decodes_the_captured_fixture() {
        // Counts confirmed earlier via the python msgpack walk:
        // dict.len() == 2, calls.len() == 10000. Hard-coding the
        // exact counts (not just ">0") means a future re-capture
        // that changes the workload silently can't slip past.
        let batch = parse_batch(FIXTURE_FLAT_CALLS_1).expect("fixture must parse");
        assert_eq!(batch.meta.schema_version, 1);
        assert_eq!(batch.dict.len(), 2);
        assert_eq!(batch.calls.len(), 10000);
    }

    #[test]
    fn parse_batch_renames_fn_to_fn_id() {
        // One dict entry, one call referencing it via wire `fn`.
        // After decode, the Rust field is `fn_id`.
        let body = build_full_batch(
            &TestMeta::valid(),
            vec![TestDictEntry::sample(7)],
            vec![TestCall::sample(1, 0, 7)],
        );
        let batch = parse_batch(&body).expect("synthetic batch must parse");
        assert_eq!(batch.calls.len(), 1);
        assert_eq!(
            batch.calls[0].fn_id, 7,
            "rename should expose `fn` as `fn_id`"
        );
    }

    #[test]
    fn parse_batch_with_empty_dict_and_calls_round_trips() {
        let body = build_full_batch(&TestMeta::valid(), vec![], vec![]);
        let batch = parse_batch(&body).expect("empty batch must parse");
        assert!(batch.dict.is_empty());
        assert!(batch.calls.is_empty());
    }

    #[test]
    fn parse_batch_rejects_non_msgpack_bytes() {
        let err = parse_batch(b"hello, world").expect_err("garbage must fail");
        assert!(matches!(err, WireError::Parse(_)));
    }

    #[test]
    fn parse_batch_rejects_missing_required_field_on_call() {
        // Build a call missing `abnormal_exit`. We define a parallel
        // struct that omits that field; rmp_serde::to_vec_named
        // will produce a call record without it.
        #[derive(Serialize)]
        struct PartialCall {
            call_id: u32,
            parent: u32,
            #[serde(rename = "fn")]
            fn_id: u32,
            depth: u32,
            t_in: i64,
            t_out: i64,
            cpu_u: i64,
            cpu_s: i64,
            mem_in: i64,
            mem_out: i64,
            // abnormal_exit deliberately omitted
        }
        #[derive(Serialize)]
        struct TestBatch {
            meta: TestMeta,
            dict: Vec<TestDictEntry>,
            calls: Vec<PartialCall>,
        }
        let body = rmp_serde::to_vec_named(&TestBatch {
            meta: TestMeta::valid(),
            dict: vec![],
            calls: vec![PartialCall {
                call_id: 1,
                parent: 0,
                fn_id: 1,
                depth: 0,
                t_in: 0,
                t_out: 1,
                cpu_u: 0,
                cpu_s: 0,
                mem_in: 0,
                mem_out: 0,
            }],
        })
        .unwrap();
        let err = parse_batch(&body).expect_err("missing call field must fail");
        assert!(matches!(err, WireError::Parse(_)));
    }

    #[test]
    fn parse_batch_ignores_extra_top_level_key() {
        #[derive(Serialize)]
        struct BatchPlusExtra {
            meta: TestMeta,
            dict: Vec<TestDictEntry>,
            calls: Vec<TestCall>,
            future_field: String,
        }
        let body = rmp_serde::to_vec_named(&BatchPlusExtra {
            meta: TestMeta::valid(),
            dict: vec![],
            calls: vec![],
            future_field: "v1-additive".into(),
        })
        .unwrap();
        let batch = parse_batch(&body).expect("extra top-level key must be ignored");
        assert_eq!(batch.meta.schema_version, 1);
    }

    #[test]
    fn parse_batch_ignores_extra_key_inside_a_call() {
        #[derive(Serialize)]
        struct CallPlusExtra {
            call_id: u32,
            parent: u32,
            #[serde(rename = "fn")]
            fn_id: u32,
            depth: u32,
            t_in: i64,
            t_out: i64,
            cpu_u: i64,
            cpu_s: i64,
            mem_in: i64,
            mem_out: i64,
            abnormal_exit: bool,
            future_per_call_field: String,
        }
        #[derive(Serialize)]
        struct TestBatch {
            meta: TestMeta,
            dict: Vec<TestDictEntry>,
            calls: Vec<CallPlusExtra>,
        }
        let body = rmp_serde::to_vec_named(&TestBatch {
            meta: TestMeta::valid(),
            dict: vec![TestDictEntry::sample(1)],
            calls: vec![CallPlusExtra {
                call_id: 1,
                parent: 0,
                fn_id: 1,
                depth: 0,
                t_in: 0,
                t_out: 1,
                cpu_u: 0,
                cpu_s: 0,
                mem_in: 0,
                mem_out: 0,
                abnormal_exit: false,
                future_per_call_field: "ignored".into(),
            }],
        })
        .unwrap();
        let batch = parse_batch(&body).expect("extra in-Call key must be ignored");
        assert_eq!(batch.calls.len(), 1);
        assert_eq!(batch.calls[0].fn_id, 1);
    }

    #[test]
    fn parse_batch_never_panics_on_malformed_inputs() {
        // A small hand-curated set of malformed inputs. The
        // function MUST return `Err`, never panic.
        let cases: &[&[u8]] = &[
            b"", // empty
            b"\xff\xff\xff\xff",
            b"hello",
            // A truncated msgpack header (fixmap of 3 fields, then nothing).
            &[0x83],
            // A top-level array of one int — wrong top-level shape.
            &[0x91, 0x01],
        ];
        for (i, body) in cases.iter().enumerate() {
            let result = parse_batch(body);
            assert!(
                result.is_err(),
                "case {i} ({:?}) should not parse to Ok",
                body
            );
        }
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
