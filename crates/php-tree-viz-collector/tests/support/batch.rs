//! Synthetic v1 batch builders shared across integration tests.
//!
//! Mirrors `build_test_batch_with_chain` from `tests/http_skeleton.rs`.
//! Kept here so the new test binaries (disk_usage, observability) can
//! call it without dragging in the entire http_skeleton module tree.

use serde::Serialize;

pub const ALL_ZERO_TRACE_ID: &str = "00000000-0000-0000-0000-000000000000";

/// One top-level call + one direct child. Aggregator folds both into
/// non-pending nodes; the resulting trace finalizes with
/// `cpu_snapshot_available = true` (both calls have non-zero
/// `cpu_u`).
pub fn build_test_batch_with_chain(host: &str, pid: u64, start_time: i64) -> Vec<u8> {
    #[derive(Serialize)]
    struct TestMeta<'a> {
        schema_version: u32,
        trace_id: &'a str,
        host: &'a str,
        pid: u64,
        start_time: i64,
        sapi: &'a str,
        uri_or_script: &'a str,
        dropped_records: u64,
    }
    #[derive(Serialize)]
    struct TestDictEntry<'a> {
        fn_id: u32,
        fqn: &'a str,
        file: &'a str,
        line: u32,
        kind: u8,
    }
    #[derive(Serialize)]
    struct TestCall {
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
    }
    #[derive(Serialize)]
    struct TestBatch<'a> {
        meta: TestMeta<'a>,
        dict: Vec<TestDictEntry<'a>>,
        calls: Vec<TestCall>,
    }
    rmp_serde::to_vec_named(&TestBatch {
        meta: TestMeta {
            schema_version: 1,
            trace_id: ALL_ZERO_TRACE_ID,
            host,
            pid,
            start_time,
            sapi: "cli",
            uri_or_script: "/tmp/chain.php",
            dropped_records: 0,
        },
        dict: vec![
            TestDictEntry {
                fn_id: 1,
                fqn: "ns\\top",
                file: "/tmp/chain.php",
                line: 1,
                kind: 0,
            },
            TestDictEntry {
                fn_id: 2,
                fqn: "ns\\child",
                file: "/tmp/chain.php",
                line: 10,
                kind: 0,
            },
        ],
        calls: vec![
            TestCall {
                call_id: 1,
                parent: 2,
                fn_id: 2,
                depth: 2,
                t_in: 100,
                t_out: 150,
                cpu_u: 5,
                cpu_s: 2,
                mem_in: 0,
                mem_out: 1024,
                abnormal_exit: false,
            },
            TestCall {
                call_id: 2,
                parent: 0,
                fn_id: 1,
                depth: 1,
                t_in: 0,
                t_out: 200,
                cpu_u: 20,
                cpu_s: 5,
                mem_in: 0,
                mem_out: 4096,
                abnormal_exit: false,
            },
        ],
    })
    .unwrap()
}
