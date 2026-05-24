//! Hot-path micro-benchmarks for the collector's decoder.
//!
//! Two cases: `parse_batch_only` measures the pure
//! `wire::parse_batch` cost; `parse_and_record` extends that with
//! `Storage::record_batch` over a fresh temp data directory per
//! iteration. The steady-state hot-path number is in `parse_batch_only`
//! — `parse_and_record` includes filesystem create cost, so it should
//! always be slower.
//!
//! Run with `cargo bench --bench decode_batch`. Output is criterion's
//! default format; this binary is **not** gated by CI. It exists to
//! make regressions measurable in future changes.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};

mod synth {
    //! Re-implementation of the `build_test_batch_with_chain` helper
    //! from `tests/http_skeleton.rs`, kept private to the bench so
    //! the bench compiles standalone (cargo's bench target cannot
    //! reach into the integration-test module tree).
    use rmp_serde::Serializer;
    use serde::Serialize;

    pub fn build_batch(host: &str, pid: u64, start_time: i64) -> Vec<u8> {
        let mut buf = Vec::with_capacity(512);
        let body = BatchPayload {
            meta: MetaShape {
                schema_version: 1,
                trace_id: "00000000-0000-0000-0000-000000000000".to_owned(),
                host: host.to_owned(),
                pid,
                start_time,
                sapi: "cli".to_owned(),
                uri_or_script: "bench.php".to_owned(),
                dropped_records: 0,
            },
            dict: vec![
                DictShape::new(1, "main", "/x/main.php", 1, 0),
                DictShape::new(2, "child", "/x/main.php", 10, 0),
            ],
            // Child returns before parent; aggregator folds both.
            calls: vec![
                CallShape::new(2, 1, 2, 1, 100, 200),
                CallShape::new(1, 0, 1, 0, 0, 300),
            ],
        };
        body.serialize(&mut Serializer::new(&mut buf)).unwrap();
        buf
    }

    #[derive(Serialize)]
    struct BatchPayload {
        meta: MetaShape,
        dict: Vec<DictShape>,
        calls: Vec<CallShape>,
    }

    #[derive(Serialize)]
    struct MetaShape {
        schema_version: u32,
        trace_id: String,
        host: String,
        pid: u64,
        start_time: i64,
        sapi: String,
        uri_or_script: String,
        dropped_records: u64,
    }

    #[derive(Serialize)]
    struct DictShape {
        fn_id: u32,
        fqn: String,
        file: String,
        line: u32,
        kind: u32,
    }

    impl DictShape {
        fn new(fn_id: u32, fqn: &str, file: &str, line: u32, kind: u32) -> Self {
            Self {
                fn_id,
                fqn: fqn.to_owned(),
                file: file.to_owned(),
                line,
                kind,
            }
        }
    }

    #[derive(Serialize)]
    struct CallShape {
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

    impl CallShape {
        fn new(call_id: u32, parent: u32, fn_id: u32, depth: u32, t_in: i64, t_out: i64) -> Self {
            Self {
                call_id,
                parent,
                fn_id,
                depth,
                t_in,
                t_out,
                cpu_u: 0,
                cpu_s: 0,
                mem_in: 0,
                mem_out: 0,
                abnormal_exit: false,
            }
        }
    }
}

/// Compact criterion config — the bench is for spotting regressions
/// across changes, not for high-precision absolute numbers. Trimming
/// `sample_size` and `measurement_time` keeps total bench wall time
/// under the spec's "<60 s on the developer's machine" ceiling
/// (tasks.md §7.4) even when SQLite's bundled C build dominates the
/// first invocation.
fn quick_criterion() -> Criterion {
    use std::time::Duration;
    Criterion::default()
        .sample_size(20)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(3))
        .configure_from_args()
}

fn parse_batch_only(c: &mut Criterion) {
    let body = synth::build_batch("bench-host", 42, 1_700_000_000_000_000_000);
    c.bench_function("parse_batch_only", |b| {
        b.iter(|| {
            let parsed = php_tree_viz_collector::wire::parse_batch(black_box(&body))
                .expect("synthetic batch parses");
            black_box(parsed);
        });
    });
}

fn parse_and_record(c: &mut Criterion) {
    use php_tree_viz_collector::http::BatchSubmission;
    use php_tree_viz_collector::storage::Storage;
    use php_tree_viz_collector::tracekey::TraceKey;

    let body = synth::build_batch("bench-host", 43, 1_700_000_000_000_000_000);
    let key = TraceKey::from_raw("00000000000000000000000000000043");

    c.bench_function("parse_and_record", |b| {
        b.iter_batched(
            || {
                let dir = tempfile::tempdir().expect("tempdir");
                let traces = dir.path().join("traces");
                std::fs::create_dir_all(&traces).unwrap();
                let storage = Storage::open(dir.path(), traces).expect("storage open");
                (dir, storage)
            },
            |(dir, mut storage)| {
                let parsed =
                    php_tree_viz_collector::wire::parse_batch(black_box(&body)).expect("parse");
                let submission = BatchSubmission {
                    path: dir.path().join("ignored.msgpack"),
                    trace_key: key.clone(),
                };
                storage
                    .record_batch(&submission, &parsed, 1_700_000_000_000_000_001)
                    .expect("record_batch");
                black_box(&storage);
            },
            BatchSize::PerIteration,
        );
    });
}

criterion_group! {
    name = benches;
    config = quick_criterion();
    targets = parse_batch_only, parse_and_record
}
criterion_main!(benches);
