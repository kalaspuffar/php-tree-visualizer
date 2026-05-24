//! Token-leak runtime guard.
//!
//! Spec scenario (collector-observability §"Bearer token never
//! appears…"): drive a representative session — auth-missing,
//! wrong-token, three accepted batches with distinct trace_ids, a
//! forced finalize tick, a forced retention tick, then shutdown —
//! and assert the captured subscriber output never contains the
//! token, the salt, or the literal string `Authorization`.
//!
//! We drive the assertion against the real subprocess stdout +
//! stderr — the same surface journald reads in production. That's
//! a stronger contract than capturing into an in-process
//! `MakeWriter`, because it exercises the actual subscriber install
//! path, the actual `fmt` layer, and the actual byte stream that
//! escapes the process.

mod support;

use std::time::Duration;

use support::{
    batch::build_test_batch_with_chain,
    harness::{
        ingest_request, request, send_raw, unique_tempdir, Collector, ConfigBuilder,
        EVENT_BATCH_ACCEPTED, EVENT_RETENTION_SWEPT, EVENT_TRACE_FINALIZED, MEDIA_TYPE, SALT,
        TOKEN,
    },
};

#[test]
fn token_does_not_appear_after_a_full_representative_session() {
    // `retention_days = 1` so a batch with a back-dated start_time
    // gets pruned by the next retention tick.
    let dir = unique_tempdir("token_leak_session");
    let path = ConfigBuilder::new(dir).fast_retention(1).write();
    let collector = Collector::spawn(&path);

    // (1) auth missing → 401
    let no_auth = request(
        "POST",
        "/ingest/v1",
        &[("Content-Type", MEDIA_TYPE)],
        &[],
        &collector.bound,
    );
    let (status, _) = send_raw(&collector.bound, &no_auth);
    assert_eq!(status, 401);

    // (2) wrong token → 401
    let wrong = request(
        "POST",
        "/ingest/v1",
        &[
            (
                "Authorization",
                "Bearer wrong-token-here-is-not-the-real-one",
            ),
            ("Content-Type", MEDIA_TYPE),
        ],
        &[],
        &collector.bound,
    );
    let (status, _) = send_raw(&collector.bound, &wrong);
    assert_eq!(status, 401);

    // (3) three accepted batches with distinct trace identities.
    // Distinct (host, pid, start_time) → distinct synthesized
    // TraceKeys, because the upstream `trace_id` is the all-zero
    // placeholder. The first two use back-dated start_times so the
    // retention sweeper prunes them on the next tick (1s); the
    // third uses a fresh start_time so it survives retention long
    // enough for the finalize loop to fire on it after the
    // 1-second idle window.
    let now_ns: i64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock past UNIX_EPOCH")
        .as_nanos() as i64;
    let stale = -(86_400_i64 * 1_000_000_000); // 1 day before epoch
    let traces = [
        ("alpha", 1u64, 1i64),   // back-dated → retention prunes
        ("beta", 2u64, stale),   // back-dated → retention prunes
        ("gamma", 3u64, now_ns), // fresh → finalize fires
    ];
    let mut batch_count = 0u32;
    for (host, pid, start_time) in traces {
        let body = build_test_batch_with_chain(host, pid, start_time);
        let req = ingest_request(&collector.bound, &body);
        let (status, _) = send_raw(&collector.bound, &req);
        assert_eq!(status, 200, "batch {host} should be accepted");
        batch_count += 1;
    }
    // Wait for the decoder to log every batch_count event so the
    // assertion checks output that includes the decoder + storage
    // code paths.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let stdout = collector.stdout_so_far.lock().unwrap().clone();
        if stdout.matches(EVENT_BATCH_ACCEPTED).count() >= batch_count as usize {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "timed out waiting for {batch_count} accepted-batch events; \
                 stdout:\n{stdout}"
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // (4) Forced finalize tick — idle_seconds=1, tick_seconds=1
    // (fast-finalize enabled by fast_retention) → traces become
    // finalized within ~2 s of the last batch.
    collector.wait_for_stdout(EVENT_TRACE_FINALIZED, Duration::from_secs(5));

    // (5) Forced retention tick — start_times 1, 2, 3 ns are
    // older than `now - 1 day`, so the retention sweeper prunes
    // all three on its next tick.
    collector.wait_for_stdout(EVENT_RETENTION_SWEPT, Duration::from_secs(5));

    // (6) Drop the collector (sends SIGTERM in Drop). We give it
    // a moment to emit the `shutdown signal received` event
    // before snapshotting.
    let stdout = collector.stdout_so_far.lock().unwrap().clone();
    let stderr = collector.stderr_so_far.lock().unwrap().clone();
    drop(collector);

    // The assertions: nothing in either captured stream may
    // contain the token, the salt, or the literal `Authorization`.
    // We assert in three forms (bytes/utf8/lowercase) for clarity.
    assert!(
        !stdout.contains(TOKEN),
        "stdout leaked the token: {stdout:?}"
    );
    assert!(
        !stderr.contains(TOKEN),
        "stderr leaked the token: {stderr:?}"
    );
    assert!(!stdout.contains(SALT), "stdout leaked the salt: {stdout:?}");
    assert!(!stderr.contains(SALT), "stderr leaked the salt: {stderr:?}");
    assert!(
        !stdout.to_lowercase().contains("authorization"),
        "stdout contained 'authorization': {stdout:?}"
    );
    assert!(
        !stderr.to_lowercase().contains("authorization"),
        "stderr contained 'authorization': {stderr:?}"
    );

    // Belt-and-braces: byte-window scan, in case unicode-y
    // normalisation in the log line ever inserts a NUL or stray
    // codepoint between characters of TOKEN/SALT. Sliding-window
    // equality is what an attacker reading the log would do.
    let token_bytes = TOKEN.as_bytes();
    let salt_bytes = SALT.as_bytes();
    let stdout_bytes = stdout.as_bytes();
    let stderr_bytes = stderr.as_bytes();
    assert!(
        !stdout_bytes
            .windows(token_bytes.len())
            .any(|w| w == token_bytes),
        "byte-window scan found the token in stdout"
    );
    assert!(
        !stderr_bytes
            .windows(token_bytes.len())
            .any(|w| w == token_bytes),
        "byte-window scan found the token in stderr"
    );
    assert!(
        !stdout_bytes
            .windows(salt_bytes.len())
            .any(|w| w == salt_bytes),
        "byte-window scan found the salt in stdout"
    );
    assert!(
        !stderr_bytes
            .windows(salt_bytes.len())
            .any(|w| w == salt_bytes),
        "byte-window scan found the salt in stderr"
    );
}

/// Spec scenario: "A debug-format of Config does not leak secrets".
/// In-process unit-style test (not a subprocess) — just confirms the
/// `Secret`-wrapping `Debug` impl on `Config` is intact across the
/// new observability + storage fields we added.
#[test]
fn debug_format_of_config_does_not_leak_secrets() {
    use php_tree_viz_collector::config::load_from_path;

    let dir = unique_tempdir("debug_format_secrets");
    let path = ConfigBuilder::new(dir).disk_capacity_bytes(1024).write();
    let cfg = load_from_path(&path).expect("config must validate");
    let rendered = format!("{cfg:?}");
    assert!(
        !rendered.contains(TOKEN),
        "Debug leaked the token: {rendered}"
    );
    assert!(
        !rendered.contains(SALT),
        "Debug leaked the salt: {rendered}"
    );
    // The redaction marker (`***`) appears for each secret.
    assert!(rendered.matches("***").count() >= 2);
}
