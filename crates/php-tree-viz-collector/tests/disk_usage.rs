//! Integration coverage for the periodic disk-usage gauge.
//!
//! These tests spawn the collector subprocess with a sub-second
//! `disk_usage_tick_seconds_test_override`, send one or more batches,
//! and inspect captured stdout for the `disk usage` event. They run
//! against the same subscriber install path operators use in
//! production.

mod support;

use std::time::Duration;

use support::{
    batch::build_test_batch_with_chain,
    harness::{
        extract_field, ingest_request, send_raw, unique_tempdir, Collector, ConfigBuilder,
        EVENT_BATCH_ACCEPTED, EVENT_DISK_USAGE,
    },
};

#[test]
fn gauge_emits_an_event_with_the_documented_field_set() {
    let dir = unique_tempdir("disk_usage_basic");
    let path = ConfigBuilder::new(dir).disk_usage_test_override(1).write();
    let collector = Collector::spawn(&path);

    // Send one batch so the index has trace_count >= 1 and the data
    // dir has measurable bytes.
    let body = build_test_batch_with_chain("disk-host", 1, 1);
    let (status, _) = send_raw(&collector.bound, &ingest_request(&collector.bound, &body));
    assert_eq!(status, 200);
    collector.wait_for_stdout(EVENT_BATCH_ACCEPTED, Duration::from_secs(5));

    // The gauge fires its first tick at startup (trace_count=0) and
    // then on every subsequent override-second tick. Poll until a
    // tick that *observed* trace_count=1 lands; cap at 5 s so a
    // wedged loop fails the test cleanly.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let line = loop {
        let stdout = collector.stdout_so_far.lock().unwrap().clone();
        if let Some(line) = stdout
            .lines()
            .filter(|l| l.contains(EVENT_DISK_USAGE))
            .find(|l| extract_field(l, "trace_count").as_deref() == Some("1"))
            .map(|s| s.to_owned())
        {
            break line;
        }
        if std::time::Instant::now() > deadline {
            panic!("no disk-usage event observed trace_count=1 within 5s; stdout:\n{stdout}");
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let bytes = extract_field(&line, "data_dir_bytes").expect("data_dir_bytes field");
    let bytes: u64 = bytes.parse().expect("data_dir_bytes parses");
    assert!(bytes > 0, "data_dir_bytes must be > 0: {line}");

    let threshold_pct = extract_field(&line, "threshold_pct").expect("threshold_pct field");
    assert_eq!(threshold_pct, "80", "default threshold_pct should be 80");

    let over = extract_field(&line, "over_threshold").expect("over_threshold field");
    // No `disk_capacity_bytes` configured → over_threshold = false.
    assert_eq!(over, "false");
}

#[test]
fn gauge_emits_warn_when_over_threshold() {
    let dir = unique_tempdir("disk_usage_over_threshold");
    // Set a deliberately tiny capacity (32 bytes) so any real
    // index.sqlite immediately trips the threshold.
    let path = ConfigBuilder::new(dir)
        .disk_capacity_bytes(32)
        .disk_usage_warn_pct(50)
        .disk_usage_test_override(1)
        .write();
    let collector = Collector::spawn(&path);

    // Send one batch so the data dir grows beyond 32 bytes (the
    // index.sqlite alone is ~24 KiB after schema apply).
    let body = build_test_batch_with_chain("disk-warn-host", 2, 2);
    let (status, _) = send_raw(&collector.bound, &ingest_request(&collector.bound, &body));
    assert_eq!(status, 200);
    collector.wait_for_stdout(EVENT_BATCH_ACCEPTED, Duration::from_secs(5));

    let stdout = collector.wait_for_stdout(EVENT_DISK_USAGE, Duration::from_secs(5));
    let line = stdout
        .lines()
        .filter(|l| l.contains(EVENT_DISK_USAGE))
        .find(|l| extract_field(l, "over_threshold").as_deref() == Some("true"))
        .expect("at least one disk-usage event must report over_threshold=true");

    // The fmt layer prefixes the line with the level token (`WARN`
    // for warn-level events; `INFO` for info-level).
    assert!(
        line.contains(" WARN "),
        "over-threshold event must be at WARN level: {line}"
    );
}

#[test]
fn gauge_ignores_files_outside_the_documented_layout() {
    let dir = unique_tempdir("disk_usage_ignore_extra");
    let data_dir = dir.join("data");
    let path = ConfigBuilder::new(dir).disk_usage_test_override(1).write();

    // Drop a deliberately *huge* rogue file outside the documented
    // layout — large enough that including it would dwarf any
    // realistic SQLite startup footprint. Use a sparse file so the
    // physical disk write is cheap; `metadata().len()` still
    // reports the logical 100 MiB, which is what walkdir reads.
    let rogue_path = data_dir.join("rogue.log");
    let rogue_size: u64 = 100 * 1024 * 1024;
    {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&rogue_path)
            .expect("create rogue.log");
        file.set_len(rogue_size).expect("set_len on rogue.log");
    }

    let collector = Collector::spawn(&path);

    // No batches sent. The very first tick observes index.sqlite +
    // its WAL/SHM + an empty tmp/ — typically ~80 KiB. If the gauge
    // were counting the rogue file too, data_dir_bytes would jump
    // to ~100 MiB. The assertion uses a generous 10 MiB ceiling so
    // it isn't brittle against SQLite page-size or WAL drift.
    let stdout = collector.wait_for_stdout(EVENT_DISK_USAGE, Duration::from_secs(5));
    let line = stdout
        .lines()
        .find(|l| l.contains(EVENT_DISK_USAGE))
        .expect("first disk-usage event must be visible");
    let bytes: u64 = extract_field(line, "data_dir_bytes")
        .expect("data_dir_bytes field")
        .parse()
        .expect("data_dir_bytes parses");
    let ceiling = 10 * 1024 * 1024;
    assert!(
        bytes < ceiling,
        "data_dir_bytes {bytes} should not include the 100 MiB rogue file (ceiling {ceiling}): {line}"
    );
}
