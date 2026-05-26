//! End-to-end integration tests for the HTTP skeleton. Each test
//! spawns the compiled binary against a temp config file, reads
//! `listening on <addr>` from its stdout to learn the bound port,
//! sends raw HTTP/1.1 bytes over a `std::net::TcpStream`, and
//! parses the response. Cleanup runs in `Drop`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_php-tree-viz-collector");
const TOKEN: &str = "PHPTVTESTTOKEN1234567890ABCDEFGH1234567890";
const SALT: &str = "PHPTVTESTSALT0987654321ZYXWVUTSR0987654321";
const MEDIA_TYPE: &str = "application/vnd.php-analyze.v1+msgpack";

/// Find the value of a `field=value` pair in a tracing-subscriber
/// text-mode log line. The fmt layer emits field values via their
/// `Display` impl with no quoting, so the value extends from `=` to
/// the next whitespace (or end of line). Returns `None` if the field
/// isn't present.
fn extract_field(line: &str, field: &str) -> Option<String> {
    let needle = format!(" {field}=");
    let pos = line.find(&needle)?;
    let value_start = pos + needle.len();
    let tail = &line[value_start..];
    let value_end = tail.find(|c: char| c.is_whitespace()).unwrap_or(tail.len());
    Some(tail[..value_end].to_owned())
}

/// Substring constants for the new structured event messages.
/// Update here, not at call sites — every test that synchronises on
/// a particular collector event uses one of these.
const EVENT_BATCH_ACCEPTED: &str = "batch accepted";
const EVENT_TRACE_FINALIZED: &str = "trace finalized";
const EVENT_RETENTION_SWEPT: &str = "retention swept";
const EVENT_CONFIG_LOADED: &str = "configuration loaded";

fn unique_tempdir(label: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir =
        std::env::temp_dir().join(format!("phptv-http-{}-{}-{}", std::process::id(), label, n,));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write a config file with a per-test data directory under `dir`.
/// Each test gets its own `<dir>/data/` and the collector's tmp
/// subdir lands at `<dir>/data/tmp/`, so concurrent runs do not
/// collide. Returns the path to the written `collector.toml`.
fn write_config(dir: &Path, bind: &str) -> PathBuf {
    write_config_with_overrides(dir, bind, dir.join("data").to_str().unwrap(), None)
}

/// Test configs use `[log] format = "text"` so the subprocess stdout
/// is human-grep-able. The structured fields land as ` field=value`
/// pairs (no quoting unless the formatter inserts it), which lets
/// the harness's substring assertions and `addr=` extraction stay
/// straightforward. Production defaults to `format = "json"`; the
/// subscriber install honours both.
const TEXT_LOG_SECTION: &str = "\n[log]\nformat = \"text\"\n";

/// Write a config with aggressive idle-finalize timings: `idle_seconds =
/// 1, tick_seconds = 1`. Used by the finalize integration tests so the
/// wait for "finalized trace …" is bounded by ~2 s + jitter instead of
/// the 30 s default. Every other setting matches `write_config`.
fn write_config_with_fast_finalize(dir: &Path, bind: &str) -> PathBuf {
    let data_dir = dir.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let body = format!(
        r#"[server]
bind = "{bind}"

[auth]
token = "{TOKEN}"
session_salt = "{SALT}"

[storage]
data_dir = "{}"
retention_days = 30

[finalize]
idle_seconds = 1
tick_seconds = 1
{TEXT_LOG_SECTION}"#,
        data_dir.display(),
    );
    let path = dir.join("collector.toml");
    std::fs::write(&path, body).unwrap();
    path
}

/// Write a config with aggressive retention timings: a caller-chosen
/// `retention_days`, `[retention] tick_seconds = 1`, and (so a slow
/// retention tick doesn't get raced by an unwanted finalize tick)
/// `[finalize] idle_seconds = 1, tick_seconds = 1`. Used by the
/// retention integration tests so the wait for `swept retention …`
/// is bounded by ~2 s + jitter instead of the hour-default tick.
fn write_config_with_fast_retention(dir: &Path, bind: &str, retention_days: u32) -> PathBuf {
    let data_dir = dir.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let body = format!(
        r#"[server]
bind = "{bind}"

[auth]
token = "{TOKEN}"
session_salt = "{SALT}"

[storage]
data_dir = "{}"
retention_days = {retention_days}

[finalize]
idle_seconds = 1
tick_seconds = 1

[retention]
tick_minutes = 60
tick_seconds = 1
{TEXT_LOG_SECTION}"#,
        data_dir.display(),
    );
    let path = dir.join("collector.toml");
    std::fs::write(&path, body).unwrap();
    path
}

/// Like `write_config` but lets a test override the `data_dir` or
/// the body-size cap. `data_dir` is *not* created automatically when
/// overridden (so tests can exercise the failure path).
fn write_config_with_overrides(
    dir: &Path,
    bind: &str,
    data_dir: &str,
    max_body_bytes: Option<u64>,
) -> PathBuf {
    // Create the data directory by default so the server can mkdir
    // its `tmp/` subdir at startup. The override-path failure tests
    // (e.g. `tmp_dir_creation_failure_exits_3`) pass a path that
    // can't be created.
    let auto_data = dir.join("data");
    if data_dir == auto_data.to_str().unwrap_or("") {
        std::fs::create_dir_all(&auto_data).unwrap();
    }
    let extra_server = max_body_bytes
        .map(|n| format!("max_body_bytes = {n}\n"))
        .unwrap_or_default();
    let body = format!(
        r#"[server]
bind = "{bind}"
{extra_server}
[auth]
token = "{TOKEN}"
session_salt = "{SALT}"

[storage]
data_dir = "{data_dir}"
retention_days = 30
{TEXT_LOG_SECTION}"#
    );
    let path = dir.join("collector.toml");
    std::fs::write(&path, body).unwrap();
    path
}

/// RAII handle that owns a spawned collector. Sends SIGTERM and then
/// waits on `Drop`, falling back to SIGKILL if the process is
/// stubborn. Captures stdout/stderr for inspection.
struct Collector {
    child: Option<Child>,
    pub bound: String,
    pub stdout_so_far: std::sync::Arc<Mutex<String>>,
    pub stderr_so_far: std::sync::Arc<Mutex<String>>,
}

impl Collector {
    fn spawn(config_path: &Path) -> Self {
        let mut child = Command::new(BIN)
            .arg("--config")
            .arg(config_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to launch the collector binary");

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let stdout_buf = std::sync::Arc::new(Mutex::new(String::new()));
        let stderr_buf = std::sync::Arc::new(Mutex::new(String::new()));

        // Drain stderr in a background thread so the pipe never fills.
        {
            let buf = stderr_buf.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => buf.lock().unwrap().push_str(&line),
                    }
                }
            });
        }

        // Drain stdout in a background thread (same shape as the
        // stderr drainer above). The "wait for listening" loop below
        // polls the captured buffer with a deadline — that way a
        // subprocess that emits "listening" in an unexpected format
        // can't wedge us on a blocking `read_line`.
        {
            let buf = stdout_buf.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => buf.lock().unwrap().push_str(&line),
                    }
                }
            });
        }

        let deadline = Instant::now() + Duration::from_secs(10);
        let bound = loop {
            // Has the binary already announced?
            let snapshot = stdout_buf.lock().unwrap().clone();
            // Tracing-subscriber text layer emits lines shaped like
            //   `<ts>  INFO target: listening addr=127.0.0.1:43210`
            // We accept the first line with the `listening` event
            // *and* an `addr=` field.
            if let Some(addr) = snapshot
                .lines()
                .find(|l| l.contains("listening"))
                .and_then(|l| extract_field(l, "addr"))
            {
                break addr;
            }
            if Instant::now() > deadline {
                let captured_err = stderr_buf.lock().unwrap().clone();
                let _ = child.kill();
                panic!(
                    "binary did not announce a `listening` event with an `addr=` field within 10s\nstdout so far:\n{snapshot}\nstderr so far:\n{captured_err}",
                );
            }
            // Has the binary already exited?
            if let Ok(Some(status)) = child.try_wait() {
                let captured_err = stderr_buf.lock().unwrap().clone();
                panic!(
                    "binary exited (status {status:?}) before announcing\nstdout:\n{snapshot}\nstderr:\n{captured_err}",
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        };

        Self {
            child: Some(child),
            bound,
            stdout_so_far: stdout_buf,
            stderr_so_far: stderr_buf,
        }
    }

    /// Block until `substring` appears in captured stdout, or panic
    /// with a diagnostic snapshot once `timeout` elapses. Used as a
    /// deterministic synchronisation point with the decoder task —
    /// fixed sleeps were flaky once aggregation's 10K-call hot path
    /// pushed end-to-end latency from ~5ms to ~200ms.
    fn wait_for_stdout(&self, substring: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let stdout = self.stdout_so_far.lock().unwrap().clone();
            if stdout.contains(substring) {
                return stdout;
            }
            if Instant::now() > deadline {
                let stderr = self.stderr_so_far.lock().unwrap().clone();
                panic!(
                    "timed out waiting for {substring:?} in stdout within {timeout:?}\nstdout:\n{stdout}\nstderr:\n{stderr}"
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Try SIGTERM first.
            let _ = Command::new("kill")
                .args(["-TERM", &child.id().to_string()])
                .status();
            // Bounded wait, then SIGKILL fallback.
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Send a single raw HTTP/1.1 request and read the full response.
/// The request includes `Connection: close`, so the server closes
/// the stream after writing the response; we read until EOF.
/// Deliberately *no* `shutdown(Write)` — half-closing the write side
/// after a `Content-Length: 0` body causes hyper to drop the
/// connection without responding.
fn send_raw(host: &str, request: &[u8]) -> (u16, String) {
    let mut stream = TcpStream::connect(host).expect("connect failed");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(request).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    parse_response(&response)
}

fn parse_response(raw: &str) -> (u16, String) {
    let mut parts = raw.splitn(2, "\r\n\r\n");
    let head = parts.next().unwrap_or("");
    let body = parts.next().unwrap_or("").to_owned();
    let status_line = head.lines().next().expect("response had no status line");
    // Expected shape: HTTP/1.1 <code> <reason>
    let code = status_line
        .split_whitespace()
        .nth(1)
        .expect("status line missing code")
        .parse()
        .expect("status code not an integer");
    (code, body)
}

fn request(method: &str, path: &str, headers: &[(&str, &str)], body: &str, host: &str) -> Vec<u8> {
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: {len}\r\n",
        method = method,
        path = path,
        host = host,
        len = body.len(),
    );
    for (name, value) in headers {
        req.push_str(&format!("{name}: {value}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    req.into_bytes()
}

// ---- Tests ----

#[test]
fn missing_authorization_returns_401() {
    let dir = unique_tempdir("missing_auth");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let req = request(
        "POST",
        "/ingest/v1",
        &[("Content-Type", MEDIA_TYPE)],
        "",
        &collector.bound,
    );
    let (status, body) = send_raw(&collector.bound, &req);
    assert_eq!(status, 401);
    assert_eq!(body, r#"{"error":"unauthorized"}"#);
}

#[test]
fn wrong_token_returns_401() {
    let dir = unique_tempdir("wrong_token");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let req = request(
        "POST",
        "/ingest/v1",
        &[
            ("Authorization", "Bearer not-the-real-token"),
            ("Content-Type", MEDIA_TYPE),
        ],
        "",
        &collector.bound,
    );
    let (status, _body) = send_raw(&collector.bound, &req);
    assert_eq!(status, 401);
}

#[test]
fn wrong_scheme_returns_401() {
    let dir = unique_tempdir("wrong_scheme");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let req = request(
        "POST",
        "/ingest/v1",
        &[
            ("Authorization", "Basic dXNlcjpwYXNz"),
            ("Content-Type", MEDIA_TYPE),
        ],
        "",
        &collector.bound,
    );
    let (status, _body) = send_raw(&collector.bound, &req);
    assert_eq!(status, 401);
}

#[test]
fn correct_token_missing_content_type_returns_415() {
    let dir = unique_tempdir("missing_ct");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let req = request(
        "POST",
        "/ingest/v1",
        &[("Authorization", &format!("Bearer {TOKEN}"))],
        "",
        &collector.bound,
    );
    let (status, body) = send_raw(&collector.bound, &req);
    assert_eq!(status, 415);
    assert_eq!(body, r#"{"error":"unsupported_content_type"}"#);
}

#[test]
fn correct_token_wrong_content_type_returns_415() {
    let dir = unique_tempdir("wrong_ct");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let req = request(
        "POST",
        "/ingest/v1",
        &[
            ("Authorization", &format!("Bearer {TOKEN}")),
            ("Content-Type", "application/json"),
        ],
        "",
        &collector.bound,
    );
    let (status, _body) = send_raw(&collector.bound, &req);
    assert_eq!(status, 415);
}

// `valid_request_returns_501_placeholder` was removed when the
// durable-ingest change retired the 501 placeholder. The new
// `valid_v1_body_returns_200_and_lands_at_canonical_path` below
// covers the post-durability success path.

#[test]
fn unknown_path_returns_404() {
    let dir = unique_tempdir("unknown_path");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let req = request("POST", "/somewhere/else", &[], "", &collector.bound);
    let (status, _body) = send_raw(&collector.bound, &req);
    assert_eq!(status, 404);
}

#[test]
fn wrong_method_on_ingest_with_valid_auth_returns_405() {
    // Auth and Content-Type middleware run before the method
    // dispatcher, so 405 is only reachable once auth has passed.
    let dir = unique_tempdir("wrong_method_authed");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let req = request(
        "GET",
        "/ingest/v1",
        &[
            ("Authorization", &format!("Bearer {TOKEN}")),
            ("Content-Type", MEDIA_TYPE),
        ],
        "",
        &collector.bound,
    );
    let (status, _body) = send_raw(&collector.bound, &req);
    assert_eq!(status, 405);
}

#[test]
fn wrong_method_on_ingest_without_auth_returns_401() {
    let dir = unique_tempdir("wrong_method_unauthed");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let req = request("GET", "/ingest/v1", &[], "", &collector.bound);
    let (status, _body) = send_raw(&collector.bound, &req);
    assert_eq!(status, 401);
}

#[test]
fn non_loopback_bind_fails_with_exit_2() {
    let dir = unique_tempdir("non_loopback");
    let path = write_config(&dir, "0.0.0.0:8088");
    let out = Command::new(BIN)
        .arg("--config")
        .arg(&path)
        .output()
        .expect("spawn failed");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("server.bind"), "stderr: {stderr:?}");
    assert!(stderr.contains("loopback"), "stderr: {stderr:?}");
    assert_eq!(stderr.lines().count(), 1);
}

#[test]
fn sigterm_triggers_clean_exit() {
    let dir = unique_tempdir("sigterm");
    let path = write_config(&dir, "127.0.0.1:0");
    let mut collector = Collector::spawn(&path);
    let pid = collector.child.as_ref().expect("child still owned").id();

    // Send SIGTERM via /bin/kill (no new deps).
    Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("kill -TERM failed to launch");

    let mut child = collector.child.take().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("process did not exit within 5s of SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(status.code(), Some(0), "non-zero exit after SIGTERM");
}

#[test]
fn token_and_authorization_never_appear_in_logs() {
    let dir = unique_tempdir("log_hygiene");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    // One good request + one bad-token request.
    let good = request(
        "POST",
        "/ingest/v1",
        &[
            ("Authorization", &format!("Bearer {TOKEN}")),
            ("Content-Type", MEDIA_TYPE),
        ],
        "",
        &collector.bound,
    );
    let _ = send_raw(&collector.bound, &good);

    let bad = request(
        "POST",
        "/ingest/v1",
        &[
            ("Authorization", "Bearer wrong-token-here"),
            ("Content-Type", MEDIA_TYPE),
        ],
        "",
        &collector.bound,
    );
    let _ = send_raw(&collector.bound, &bad);

    // Give the request-logging middleware a beat to flush.
    std::thread::sleep(Duration::from_millis(150));

    let stdout = collector.stdout_so_far.lock().unwrap().clone();
    let stderr = collector.stderr_so_far.lock().unwrap().clone();

    assert!(
        !stdout.contains(TOKEN),
        "stdout leaked the configured token: {stdout:?}"
    );
    assert!(
        !stderr.contains(TOKEN),
        "stderr leaked the configured token: {stderr:?}"
    );
    // The startup banner does mention `token=***` — that's fine; assert
    // only that the literal header name does not appear.
    assert!(
        !stdout.to_lowercase().contains("authorization"),
        "stdout contained 'authorization': {stdout:?}"
    );
    assert!(
        !stderr.to_lowercase().contains("authorization"),
        "stderr contained 'authorization': {stderr:?}"
    );
}

#[test]
fn startup_banner_is_a_redacted_summary_line() {
    let dir = unique_tempdir("banner");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    // Give the binary a beat to flush its banner; the `Collector`
    // already saw the `listening` event, but `configuration loaded`
    // is the event emitted just before it.
    std::thread::sleep(Duration::from_millis(50));
    let stdout = collector.stdout_so_far.lock().unwrap().clone();

    assert!(
        stdout.contains(EVENT_CONFIG_LOADED),
        "missing '{EVENT_CONFIG_LOADED}' in stdout: {stdout:?}"
    );
    // The new configuration-loaded event omits the token and salt
    // fields entirely — INV-2 is enforced by exclusion rather than
    // redaction. The previous summary line carried `token=***`; the
    // new event has nothing in that field-shape at all.
    assert!(
        !stdout.contains(TOKEN),
        "banner leaked the token: {stdout:?}"
    );
    assert!(!stdout.contains(SALT), "banner leaked the salt: {stdout:?}");
    // The configured path *must* appear in the event's `path` field.
    assert!(
        stdout.contains(&path.display().to_string()),
        "banner missing the configuration path: {stdout:?}"
    );
}

/// `etc/collector.toml.example` is embedded at compile time so the
/// test never resolves a relative path against the runtime CWD.
const EXAMPLE_FILE: &str = include_str!("../../../etc/collector.toml.example");

#[test]
fn example_config_file_loads_and_server_binds() {
    let token = "T".repeat(40);
    let salt = "S".repeat(40);
    let dir = unique_tempdir("example_file");
    let data_dir = dir.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    // `REPLACE_ME` is a prefix of `REPLACE_ME_TOO`, so substitute
    // the longer string first. The example pins port 8088 and the
    // operator-default data_dir; tests need ephemeral ports and a
    // writable per-test data dir.
    let body = EXAMPLE_FILE
        .replace("REPLACE_ME_TOO", &salt)
        .replace("REPLACE_ME", &token)
        .replace("127.0.0.1:8088", "127.0.0.1:0")
        .replace("/var/lib/php-tree-viz", data_dir.to_str().unwrap())
        // The example pins `format = "json"` for production. The
        // test harness extracts the bound address from the
        // `listening` event's `addr=` field, which is the
        // text-format shape — so swap to text here.
        .replace(r#"format = "json""#, r#"format = "text""#);

    let path = dir.join("collector.toml");
    std::fs::write(&path, body).unwrap();

    // Spawning the Collector implicitly asserts the binary loaded the
    // config and bound a port — if either failed, `spawn` panics.
    let _collector = Collector::spawn(&path);
}

// ============================================================
// Body-streaming tests (added by the body-streaming change)
// ============================================================

/// Build a request with explicit `Content-Length` (used to exercise
/// the fast-path 413). The body is included verbatim after headers.
fn request_with_body(
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    host: &str,
) -> Vec<u8> {
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: {len}\r\n",
        len = body.len(),
    );
    for (name, value) in headers {
        req.push_str(&format!("{name}: {value}\r\n"));
    }
    req.push_str("\r\n");
    let mut bytes = req.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

/// Build a chunked-encoded request. Each `chunks` entry becomes its
/// own chunk. The terminating `0\r\n\r\n` closes the body. Used to
/// exercise the streaming-cap path without `Content-Length`.
fn chunked_request(
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    chunks: &[&[u8]],
    host: &str,
) -> Vec<u8> {
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n",
    );
    for (name, value) in headers {
        req.push_str(&format!("{name}: {value}\r\n"));
    }
    req.push_str("\r\n");
    let mut bytes = req.into_bytes();
    for chunk in chunks {
        bytes.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
        bytes.extend_from_slice(chunk);
        bytes.extend_from_slice(b"\r\n");
    }
    bytes.extend_from_slice(b"0\r\n\r\n");
    bytes
}

fn list_partials(data_dir: &Path) -> Vec<PathBuf> {
    let tmp = data_dir.join("tmp");
    if !tmp.is_dir() {
        return Vec::new();
    }
    std::fs::read_dir(&tmp)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "partial"))
        .collect()
}

#[test]
fn content_length_above_cap_returns_413_without_partial_file() {
    let dir = unique_tempdir("cl_above_cap");
    let path = write_config_with_overrides(
        &dir,
        "127.0.0.1:0",
        dir.join("data").to_str().unwrap(),
        Some(1024),
    );
    let data_dir = dir.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let collector = Collector::spawn(&path);

    let body = vec![0u8; 4096];
    let req = request_with_body(
        "POST",
        "/ingest/v1",
        &[
            ("Authorization", &format!("Bearer {TOKEN}")),
            ("Content-Type", MEDIA_TYPE),
        ],
        &body,
        &collector.bound,
    );
    let (status, resp_body) = send_raw(&collector.bound, &req);
    assert_eq!(status, 413);
    assert_eq!(resp_body, r#"{"error":"too_large"}"#);

    // Allow the server a brief moment to flush its log line; ensure
    // no partial file ever materialised in tmp/.
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        list_partials(&data_dir).len(),
        0,
        "fast-path 413 should not create any partial files"
    );
}

#[test]
fn chunked_body_exceeding_cap_returns_413_and_cleans_up() {
    let dir = unique_tempdir("chunked_over_cap");
    let path = write_config_with_overrides(
        &dir,
        "127.0.0.1:0",
        dir.join("data").to_str().unwrap(),
        Some(1024),
    );
    let data_dir = dir.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let collector = Collector::spawn(&path);

    // Two ~2500-byte chunks → 5000 total, well over the 1024 cap.
    let chunk = vec![b'x'; 2500];
    let req = chunked_request(
        "POST",
        "/ingest/v1",
        &[
            ("Authorization", &format!("Bearer {TOKEN}")),
            ("Content-Type", MEDIA_TYPE),
        ],
        &[chunk.as_slice(), chunk.as_slice()],
        &collector.bound,
    );
    let (status, _resp_body) = send_raw(&collector.bound, &req);
    assert_eq!(status, 413);

    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        list_partials(&data_dir).len(),
        0,
        "413-on-stream should delete the partial file it created"
    );
}

// `within_cap_body_lands_on_disk_as_partial` was removed when the
// durable-ingest change started renaming partial files into
// `traces/<key>.raw/batch-NNNN.msgpack` on success. The post-
// durability equivalent is `valid_v1_body_returns_200_and_lands_at_canonical_path`,
// which uses a real captured fixture and asserts the canonical
// path. The MODIFIED "Request body is streamed to a unique tmp
// file" spec now requires the partial to *not* be observable in
// tmp/ after a 200.

// `concurrent_requests_produce_distinct_partial_files` was removed
// when the durable-ingest change renamed partial files away on
// success. The post-durability equivalent for filename uniqueness
// is `make_filename`'s unit test in `src/http/tmp.rs`. The
// post-durability equivalent for concurrent rename safety is
// `concurrent_same_trace_requests_each_get_a_unique_batch_number`
// below, which asserts that 5 concurrent same-trace requests each
// land at a distinct `batch-NNNN`.

#[test]
fn startup_wipes_pre_existing_partial_files() {
    let dir = unique_tempdir("startup_wipe");
    let data_dir = dir.join("data");
    let tmp = data_dir.join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let leftover = tmp.join("0123456789abcdef0123456789abcdef.partial");
    std::fs::write(&leftover, b"stale").unwrap();
    assert!(leftover.exists());

    let path = write_config_with_overrides(&dir, "127.0.0.1:0", data_dir.to_str().unwrap(), None);
    let _collector = Collector::spawn(&path);

    // Collector::spawn returned once `listening on …` was printed,
    // which is *after* `ensure_clean_tmp_dir`. So the partial must
    // already be gone.
    assert!(
        !leftover.exists(),
        "startup did not delete the pre-existing partial file"
    );
}

#[test]
fn tmp_dir_creation_failure_exits_3() {
    let dir = unique_tempdir("tmp_dir_failure");
    // Make `data_dir` point at a regular file so `mkdir(data_dir/tmp)`
    // can't succeed (ENOTDIR).
    let blocker = dir.join("blocker");
    std::fs::write(&blocker, b"not a dir").unwrap();
    let path = write_config_with_overrides(&dir, "127.0.0.1:0", blocker.to_str().unwrap(), None);

    let out = Command::new(BIN)
        .arg("--config")
        .arg(&path)
        .output()
        .expect("spawn failed");
    assert_eq!(out.status.code(), Some(3), "expected http exit code 3");
    // The tmp-dir failure surfaces *after* the subscriber is
    // installed, so the error event lands on stdout via tracing's
    // fmt layer. (Pre-subscriber failures still go to stderr —
    // those are tested in cli.rs.)
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stdout.contains(blocker.to_str().unwrap()),
        "stdout should name the unreachable path; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("tmp"),
        "stdout should mention tmp; stdout:\n{stdout}"
    );
    // The failing event itself is one line — we don't assert on
    // total line count because the `configuration loaded` info
    // event also fires before the failure.
    assert!(
        stdout
            .lines()
            .filter(|l| l.contains("http server failed"))
            .count()
            == 1,
        "expected exactly one `http server failed` event; stdout:\n{stdout}"
    );
}

// `log_line_includes_body_bytes_for_within_cap_request` was
// removed when the durable-ingest change retired the 501
// placeholder. The post-durability equivalent is
// `success_path_log_line_carries_body_bytes_and_status_200`
// below, which sends a valid v1 batch and asserts the log line
// carries the body byte count plus `status=200`.

#[test]
fn log_line_includes_body_bytes_for_413_abort() {
    let dir = unique_tempdir("log_body_bytes_413");
    let path = write_config_with_overrides(
        &dir,
        "127.0.0.1:0",
        dir.join("data").to_str().unwrap(),
        Some(1024),
    );
    std::fs::create_dir_all(dir.join("data")).unwrap();
    let collector = Collector::spawn(&path);

    // Chunked body so we go down the running-count path (the
    // Content-Length fast-path would log body_bytes=0).
    let chunk = vec![b'x'; 2500];
    let req = chunked_request(
        "POST",
        "/ingest/v1",
        &[
            ("Authorization", &format!("Bearer {TOKEN}")),
            ("Content-Type", MEDIA_TYPE),
        ],
        &[chunk.as_slice(), chunk.as_slice()],
        &collector.bound,
    );
    let (status, _) = send_raw(&collector.bound, &req);
    assert_eq!(status, 413);

    std::thread::sleep(Duration::from_millis(150));
    let stdout = collector.stdout_so_far.lock().unwrap().clone();

    // Find the log line for this request and parse its body_bytes.
    let line = stdout
        .lines()
        .find(|l| l.contains("status=413") && l.contains("body_bytes="))
        .unwrap_or_else(|| panic!("no 413 log line in stdout: {stdout:?}"));
    let n: u64 = line
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("body_bytes="))
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("could not parse body_bytes from: {line}"));
    assert!(
        n >= 1024,
        "expected body_bytes >= cap (1024) on 413 abort, got {n}"
    );
}

// ============================================================
// Durable-ingest tests (added by the durable-ingest change)
// ============================================================

use serde::Serialize;

/// A captured real batch from the `flat_calls` workload. The source
/// is the gitignored `handover/batches/` directory; the copy under
/// `tests/fixtures/` is what's tracked in git and visible to CI.
/// `trace_id` is all-zero in the capture, so the TraceKey synthesis
/// path is exercised; `host`, `pid`, and `start_time` come from
/// the original capture.
const FIXTURE_FLAT_CALLS_1: &[u8] = include_bytes!("fixtures/flat_calls/batch-0001.msgpack");

/// Build a synthetic batch via `rmp_serde::to_vec_named`. Used by
/// edge-case tests that need a specific `schema_version` or
/// `trace_id` value that the captured fixtures don't carry.
fn build_test_batch(
    schema_version: u32,
    trace_id: &str,
    host: &str,
    pid: u64,
    start_time: i64,
) -> Vec<u8> {
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
    struct TestBatch<'a> {
        meta: TestMeta<'a>,
        dict: Vec<()>,
        calls: Vec<()>,
    }
    rmp_serde::to_vec_named(&TestBatch {
        meta: TestMeta {
            schema_version,
            trace_id,
            host,
            pid,
            start_time,
            sapi: "cli",
            uri_or_script: "/tmp/test.php",
            dropped_records: 0,
        },
        dict: vec![],
        calls: vec![],
    })
    .unwrap()
}

const ALL_ZERO_TRACE_ID: &str = "00000000-0000-0000-0000-000000000000";

/// Build a tiny MessagePack batch with one top-level call and one
/// child of that call — exercises the aggregation path end-to-end
/// without depending on captured fixtures, which all happen to be
/// mid-trace snapshots (every chain roots on the script body whose
/// exit hasn't reached us yet).
///
/// Wire shape:
/// - dict: two entries (`fn_id = 1` for the parent, `fn_id = 2` for
///   the child).
/// - calls (exit order, so child first):
///   - `{ call_id: 1, parent: 2, fn: 2, wall: 50 }` (child)
///   - `{ call_id: 2, parent: 0, fn: 1, wall: 200 }` (top-level)
fn build_test_batch_with_chain(host: &str, pid: u64, start_time: i64) -> Vec<u8> {
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

/// Build a tiny MessagePack batch with one top-level Call whose
/// `fn_id` is **not** in `dict` — the canonical DQ-1 shape.
///
/// Post `tolerate-out-of-order-batches`: aggregation does NOT write
/// an anomaly row during `record_batch`. Instead it parks the Call
/// in `pending_calls` expecting a late dict entry. The
/// `unresolved_fn` anomaly row appears only at idle-finalize, for
/// any pending row whose fn_id never made it into `dict`.
fn build_test_batch_with_unresolved_fn(host: &str, pid: u64, start_time: i64) -> Vec<u8> {
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
        dict: Vec<()>, // deliberately empty — `fn_id=99` won't resolve
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
            uri_or_script: "/tmp/dq1.php",
            dropped_records: 0,
        },
        dict: vec![],
        calls: vec![TestCall {
            call_id: 1,
            parent: 0,
            fn_id: 99,
            depth: 1,
            t_in: 0,
            t_out: 100,
            cpu_u: 0,
            cpu_s: 0,
            mem_in: 0,
            mem_out: 0,
            abnormal_exit: false,
        }],
    })
    .unwrap()
}

/// Build a tiny MessagePack batch with one top-level Call where
/// `t_out < t_in` — the canonical DQ-3 shape. The Call folds into
/// `nodes` with `total_wall_ns = 0` (via saturating_sub) and
/// aggregation writes one anomaly row with
/// `kind = 'inverted_time'` attached to the resulting node.
fn build_test_batch_with_inverted_time(host: &str, pid: u64, start_time: i64) -> Vec<u8> {
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
            uri_or_script: "/tmp/dq3.php",
            dropped_records: 0,
        },
        dict: vec![TestDictEntry {
            fn_id: 7,
            fqn: "ns\\inverted",
            file: "/tmp/dq3.php",
            line: 1,
            kind: 0,
        }],
        calls: vec![TestCall {
            call_id: 1,
            parent: 0,
            fn_id: 7,
            depth: 1,
            t_in: 500,
            t_out: 400, // inverted on purpose
            cpu_u: 0,
            cpu_s: 0,
            mem_in: 0,
            mem_out: 0,
            abnormal_exit: false,
        }],
    })
    .unwrap()
}

/// Build a tiny MessagePack batch with one Call whose `parent` is a
/// `call_id` (999) that no batch will ever produce — the canonical
/// DQ-2 shape. The Call's `fn_id` (7) IS in `dict`, so it doesn't
/// hit the DQ-1 path; it lands in `pending_calls` and stays there
/// until `finalize_trace` drains it into a `pending_parent_at_finalize`
/// anomaly.
fn build_test_batch_with_orphan_pending(host: &str, pid: u64, start_time: i64) -> Vec<u8> {
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
            uri_or_script: "/tmp/dq2.php",
            dropped_records: 0,
        },
        dict: vec![TestDictEntry {
            fn_id: 7,
            fqn: "ns\\orphan_child",
            file: "/tmp/dq2.php",
            line: 1,
            kind: 0,
        }],
        calls: vec![TestCall {
            call_id: 42,
            parent: 999, // parent never arrives → pending → DQ-2 at finalize
            fn_id: 7,
            depth: 2,
            t_in: 0,
            t_out: 100,
            cpu_u: 0, // every Call has zero CPU → cpu_snapshot_available=0
            cpu_s: 0,
            mem_in: 0,
            mem_out: 0,
            abnormal_exit: false,
        }],
    })
    .unwrap()
}

fn ingest_request(host: &str, body: &[u8]) -> Vec<u8> {
    request_with_body(
        "POST",
        "/ingest/v1",
        &[
            ("Authorization", &format!("Bearer {TOKEN}")),
            ("Content-Type", MEDIA_TYPE),
        ],
        body,
        host,
    )
}

fn list_batch_files(traces_dir: &Path, trace_key: &str) -> Vec<PathBuf> {
    let trace_dir = traces_dir.join(format!("{trace_key}.raw"));
    if !trace_dir.is_dir() {
        return Vec::new();
    }
    let mut out: Vec<_> = std::fs::read_dir(&trace_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|s| s == "msgpack")
        })
        .collect();
    out.sort();
    out
}

/// Compute the synthesised TraceKey for an all-zero trace_id.
/// Mirrors `src/tracekey.rs::synthesize` so tests can predict where
/// the batch file lands.
fn synth_trace_key(host: &str, pid: u64, start_time: i64) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(host.as_bytes());
    h.update(pid.to_le_bytes());
    h.update(start_time.to_le_bytes());
    let d = h.finalize();
    d[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

#[test]
fn valid_v1_body_returns_200_and_lands_at_canonical_path() {
    let dir = unique_tempdir("v1_durable");
    let data_dir = dir.join("data");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let req = ingest_request(&collector.bound, FIXTURE_FLAT_CALLS_1);
    let (status, resp_body) = send_raw(&collector.bound, &req);
    assert_eq!(status, 200, "expected 200; body was: {resp_body:?}");
    assert!(resp_body.is_empty(), "200 must have empty body");

    let traces_dir = data_dir.join("traces");
    std::thread::sleep(Duration::from_millis(100));
    // Filter for the raw msgpack directory only — `traces/` now
    // also holds the per-trace SQLite, its `-wal`, and its `-shm`
    // sidecars (added by storage-sqlite). The durable-ingest
    // contract is about the `.raw` directory; that's what we
    // assert on here.
    let raw_dirs: Vec<_> = std::fs::read_dir(&traces_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|s| s.ends_with(".raw"))
        })
        .collect();
    assert_eq!(
        raw_dirs.len(),
        1,
        "expected exactly one .raw trace dir under traces/, got {raw_dirs:?}"
    );
    let trace_raw = &raw_dirs[0];

    let batches: Vec<_> = std::fs::read_dir(trace_raw)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    assert_eq!(batches.len(), 1, "expected one batch file, got {batches:?}");
    assert!(batches[0].ends_with("batch-0001.msgpack"));
    assert_eq!(std::fs::read(&batches[0]).unwrap(), FIXTURE_FLAT_CALLS_1);

    let leftovers = list_partials(&data_dir);
    assert!(
        leftovers.is_empty(),
        "expected no partial files; got {leftovers:?}"
    );
}

#[test]
fn consecutive_batches_for_same_trace_number_monotonically() {
    let dir = unique_tempdir("monotonic");
    let data_dir = dir.join("data");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let body = build_test_batch(1, ALL_ZERO_TRACE_ID, "host-mono", 42, 9_999_999);
    for _ in 0..3 {
        let req = ingest_request(&collector.bound, &body);
        let (status, _) = send_raw(&collector.bound, &req);
        assert_eq!(status, 200);
    }

    std::thread::sleep(Duration::from_millis(100));
    let key = synth_trace_key("host-mono", 42, 9_999_999);
    let batches = list_batch_files(&data_dir.join("traces"), &key);
    assert_eq!(batches.len(), 3, "want 3 batches; got {batches:?}");
    let names: Vec<_> = batches
        .iter()
        .map(|p| p.file_name().unwrap().to_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        names,
        vec![
            "batch-0001.msgpack".to_owned(),
            "batch-0002.msgpack".to_owned(),
            "batch-0003.msgpack".to_owned(),
        ]
    );
}

#[test]
fn schema_version_2_returns_422_and_deletes_partial() {
    let dir = unique_tempdir("schema_v2");
    let data_dir = dir.join("data");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let body = build_test_batch(2, ALL_ZERO_TRACE_ID, "h", 1, 1);
    let req = ingest_request(&collector.bound, &body);
    let (status, resp_body) = send_raw(&collector.bound, &req);
    assert_eq!(status, 422);
    assert_eq!(
        resp_body,
        r#"{"error":"unsupported_schema_version","got":2}"#
    );

    std::thread::sleep(Duration::from_millis(50));
    assert!(
        list_partials(&data_dir).is_empty(),
        "422 must delete the partial file"
    );
    let traces_dir = data_dir.join("traces");
    let dirs: Vec<_> = std::fs::read_dir(&traces_dir)
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert!(
        dirs.is_empty(),
        "422 must not create a trace dir; got {dirs:?}"
    );
}

#[test]
fn non_msgpack_body_returns_400() {
    let dir = unique_tempdir("not_msgpack");
    let data_dir = dir.join("data");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let req = ingest_request(&collector.bound, b"hello, world");
    let (status, resp_body) = send_raw(&collector.bound, &req);
    assert_eq!(status, 400);
    assert!(
        resp_body.starts_with(r#"{"error":"malformed_msgpack""#),
        "wrong body: {resp_body:?}"
    );

    std::thread::sleep(Duration::from_millis(50));
    assert!(list_partials(&data_dir).is_empty());
}

#[test]
fn body_missing_meta_returns_400() {
    let dir = unique_tempdir("no_meta");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

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

    let req = ingest_request(&collector.bound, &body);
    let (status, resp_body) = send_raw(&collector.bound, &req);
    assert_eq!(status, 400);
    assert!(resp_body.starts_with(r#"{"error":"malformed_msgpack""#));
}

#[test]
fn concurrent_same_trace_requests_each_get_a_unique_batch_number() {
    let dir = unique_tempdir("concurrent_same");
    let data_dir = dir.join("data");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let body = build_test_batch(1, ALL_ZERO_TRACE_ID, "host-concur", 7, 1_234_567);
    let bound = collector.bound.clone();

    let mut handles = Vec::with_capacity(5);
    for _ in 0..5 {
        let bound = bound.clone();
        let body = body.clone();
        handles.push(std::thread::spawn(move || {
            let req = ingest_request(&bound, &body);
            send_raw(&bound, &req)
        }));
    }
    for h in handles {
        let (status, _) = h.join().unwrap();
        assert_eq!(status, 200);
    }

    std::thread::sleep(Duration::from_millis(100));
    let key = synth_trace_key("host-concur", 7, 1_234_567);
    let batches = list_batch_files(&data_dir.join("traces"), &key);
    assert_eq!(batches.len(), 5, "want 5 batches; got {batches:?}");
    let names: std::collections::BTreeSet<_> = batches
        .iter()
        .map(|p| p.file_name().unwrap().to_owned())
        .collect();
    assert_eq!(names.len(), 5, "names must be distinct");
    for i in 1..=5 {
        let expected = format!("batch-{i:04}.msgpack");
        assert!(
            names.iter().any(|n| n.to_str() == Some(&expected)),
            "missing {expected}: {names:?}"
        );
    }
}

#[test]
fn numbering_continues_across_restart() {
    let dir = unique_tempdir("restart_numbering");
    let data_dir = dir.join("data");
    let path = write_config(&dir, "127.0.0.1:0");

    let host = "host-restart";
    let pid = 11;
    let start_time = 4_242_424_242;
    let key = synth_trace_key(host, pid, start_time);

    // Pre-create batch-0001 to simulate a previous run.
    let trace_dir = data_dir.join("traces").join(format!("{key}.raw"));
    std::fs::create_dir_all(&trace_dir).unwrap();
    std::fs::write(trace_dir.join("batch-0001.msgpack"), b"earlier").unwrap();

    let collector = Collector::spawn(&path);

    let body = build_test_batch(1, ALL_ZERO_TRACE_ID, host, pid, start_time);
    let req = ingest_request(&collector.bound, &body);
    let (status, _) = send_raw(&collector.bound, &req);
    assert_eq!(status, 200);

    std::thread::sleep(Duration::from_millis(100));
    let batches = list_batch_files(&data_dir.join("traces"), &key);
    assert_eq!(batches.len(), 2);
    assert!(batches[1].ends_with("batch-0002.msgpack"));
}

#[test]
fn success_path_log_line_carries_body_bytes_and_status_200() {
    let dir = unique_tempdir("log_200");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let body = build_test_batch(1, ALL_ZERO_TRACE_ID, "h", 1, 1);
    let body_len = body.len();
    let req = ingest_request(&collector.bound, &body);
    let (status, _) = send_raw(&collector.bound, &req);
    assert_eq!(status, 200);

    std::thread::sleep(Duration::from_millis(150));
    let stdout = collector.stdout_so_far.lock().unwrap().clone();
    let line = stdout
        .lines()
        .find(|l| l.contains("status=200") && l.contains("body_bytes="))
        .unwrap_or_else(|| panic!("no 200 log line: {stdout:?}"));
    let expected_field = format!("body_bytes={body_len}");
    assert!(
        line.contains(&expected_field),
        "missing {expected_field} in: {line}"
    );
}

// ============================================================
// Bounded-mpsc tests (added by the bounded-mpsc change)
// ============================================================

/// Write a config file that also sets `server.queue_capacity`.
/// Same `<dir>/data/` auto-creation as `write_config`.
fn write_config_with_queue_capacity(dir: &Path, bind: &str, queue_capacity: u32) -> PathBuf {
    let data_dir = dir.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let body = format!(
        r#"[server]
bind = "{bind}"
queue_capacity = {queue_capacity}

[auth]
token = "{TOKEN}"
session_salt = "{SALT}"

[storage]
data_dir = "{data_dir}"
retention_days = 30
{TEXT_LOG_SECTION}"#,
        data_dir = data_dir.display(),
    );
    let path = dir.join("collector.toml");
    std::fs::write(&path, body).unwrap();
    path
}

/// Build a v1 batch whose first `Call` has `abnormal_exit` encoded
/// as an integer instead of a bool. `parse_meta` (the request-path
/// parser) only materialises the `meta` block, so it'll accept this
/// body and the request will return `200`. The decoder's
/// `parse_batch` then fails with a type-mismatch error on the
/// `abnormal_exit` field — exactly the malformed-on-disk path we
/// want to exercise.
fn build_batch_with_broken_call(host: &str, pid: u64, start_time: i64) -> Vec<u8> {
    use serde::Serialize;
    #[derive(Serialize)]
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
    // A `Call` lookalike whose `abnormal_exit` is `u8`, not bool.
    #[derive(Serialize)]
    struct BrokenCall {
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
        abnormal_exit: u8, // ← wire spec says bool; this is the corruption
    }
    #[derive(Serialize)]
    struct BrokenBatch {
        meta: TestMeta,
        dict: Vec<()>,
        calls: Vec<BrokenCall>,
    }
    rmp_serde::to_vec_named(&BrokenBatch {
        meta: TestMeta {
            schema_version: 1,
            trace_id: ALL_ZERO_TRACE_ID.into(),
            host: host.into(),
            pid,
            start_time,
            sapi: "cli".into(),
            uri_or_script: "/tmp/x.php".into(),
            dropped_records: 0,
        },
        dict: vec![],
        calls: vec![BrokenCall {
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
            abnormal_exit: 1, // an integer where a bool is required
        }],
    })
    .unwrap()
}

#[test]
fn decoder_logs_error_and_continues_after_malformed_batch() {
    let dir = unique_tempdir("decoder_recovery");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    // First request: broken Call. `parse_meta` accepts it (only
    // looks at `meta`), so the 200 happens; the decoder then trips
    // on `abnormal_exit`-as-int and logs to stderr.
    let broken = build_batch_with_broken_call("host-broken", 11, 1_111_111);
    let (status, _) = send_raw(&collector.bound, &ingest_request(&collector.bound, &broken));
    assert_eq!(
        status, 200,
        "broken-call body still 200s — parse_meta only reads `meta`"
    );

    // Second request: well-formed batch (empty dict + calls). The
    // decoder must still be alive and process it. Use a different
    // host so the synthesised trace_key differs from the first.
    let good = build_test_batch(1, ALL_ZERO_TRACE_ID, "host-good", 12, 2_222_222);
    let (status, _) = send_raw(&collector.bound, &ingest_request(&collector.bound, &good));
    assert_eq!(status, 200);

    // Let both decoder iterations run.
    std::thread::sleep(Duration::from_millis(250));

    let stdout = collector.stdout_so_far.lock().unwrap().clone();

    // The tracing subscriber routes every event — info or warn — to
    // stdout. The decoder-failure event ("decoder failure") lands
    // there alongside the batch-accepted events. (Pre-subscriber
    // `eprintln!`s are the only thing that ever reaches stderr.)
    let broken_key = synth_trace_key("host-broken", 11, 1_111_111);
    assert!(
        stdout
            .lines()
            .any(|l| l.contains("decoder failure") && l.contains(&broken_key)),
        "expected a `decoder failure` event for the broken trace; \
         stdout was:\n{stdout}",
    );

    // Stdout should have a `batch accepted` line for the second
    // (well-formed) request, proving the decoder kept running.
    let good_key = synth_trace_key("host-good", 12, 2_222_222);
    assert!(
        stdout
            .lines()
            .any(|l| l.contains(EVENT_BATCH_ACCEPTED) && l.contains(&good_key)),
        "expected a `batch accepted` line for the good trace after the broken one; \
         stdout was:\n{stdout}",
    );

    // And no `batch accepted` line for the broken trace.
    assert!(
        !stdout
            .lines()
            .any(|l| l.contains(EVENT_BATCH_ACCEPTED) && l.contains(&broken_key)),
        "batch accepted line for the broken trace shouldn't exist; stdout: {stdout}",
    );
}

#[test]
fn successful_request_is_logged_as_decoded_by_the_receiver() {
    // Renamed from `..._dequeued_...` when the `wire-decoder` change
    // replaced the placeholder receiver with a real decoder. The log
    // line now carries the decoded counts (`dict` / `calls`) instead
    // of just acknowledging the dequeue.
    let dir = unique_tempdir("decoded");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let host = "host-decode";
    let pid: u64 = 4242;
    let start_time: i64 = 17_000_000_000_000_000;
    let body = build_test_batch(1, ALL_ZERO_TRACE_ID, host, pid, start_time);

    let req = ingest_request(&collector.bound, &body);
    let (status, _) = send_raw(&collector.bound, &req);
    assert_eq!(status, 200);

    // Decoder runs in a background tokio task; give it a beat.
    std::thread::sleep(Duration::from_millis(200));
    let stdout = collector.stdout_so_far.lock().unwrap().clone();
    let key = synth_trace_key(host, pid, start_time);
    let expected_field = format!("trace_key={key}");
    let line = stdout
        .lines()
        .find(|l| l.contains(EVENT_BATCH_ACCEPTED) && l.contains(&expected_field))
        .unwrap_or_else(|| {
            panic!("no matching decoded line for {expected_field} in stdout: {stdout:?}")
        });
    // The new `batch accepted` event no longer carries a `path`
    // field (the raw path is implementation detail of where the
    // commit lands; the operator-visible identity is `trace_key`).
    // `build_test_batch` ships `dict = []` and `calls = []`, so the
    // event should show counts of 0 for both. The structured field
    // names are `dict_entries` and `call_count` (per F-1.10).
    assert!(
        line.contains("dict_entries=0"),
        "missing dict_entries=0 in: {line}"
    );
    assert!(
        line.contains("call_count=0"),
        "missing call_count=0 in: {line}"
    );
}

#[test]
fn five_concurrent_same_trace_requests_with_capacity_1_produce_503s() {
    let dir = unique_tempdir("backpressure_capacity_1");
    let data_dir = dir.join("data");
    let path = write_config_with_queue_capacity(&dir, "127.0.0.1:0", 1);
    let collector = Collector::spawn(&path);

    // All five threads target the same synthesised trace key. The
    // permit is held across each request's commit_partial (~10 ms
    // of fsync), so at least four of the five threads observe
    // queue_capacity == 0 and get 503.
    let body = build_test_batch(1, ALL_ZERO_TRACE_ID, "host-bp", 1234, 5_555_555);
    let bound = collector.bound.clone();

    let mut handles = Vec::with_capacity(5);
    for _ in 0..5 {
        let bound = bound.clone();
        let body = body.clone();
        handles.push(std::thread::spawn(move || {
            let req = ingest_request(&bound, &body);
            send_raw(&bound, &req)
        }));
    }
    let mut statuses = Vec::with_capacity(5);
    for h in handles {
        let (status, _) = h.join().unwrap();
        statuses.push(status);
    }

    let n_503 = statuses.iter().filter(|&&s| s == 503).count();
    let n_200 = statuses.iter().filter(|&&s| s == 200).count();
    assert!(
        n_503 >= 1,
        "expected at least one 503 backpressure; got statuses {statuses:?}"
    );
    assert_eq!(
        n_200 + n_503,
        5,
        "every response must be either 200 or 503; got {statuses:?}"
    );

    // Some 200s might have landed; the partial files for those are
    // renamed away. No `.partial` should remain regardless.
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        list_partials(&data_dir).is_empty(),
        "503 paths must delete their partials"
    );
}

#[test]
fn backpressure_503_leaves_no_partial_file_in_tmp() {
    let dir = unique_tempdir("backpressure_no_partial");
    let data_dir = dir.join("data");
    let path = write_config_with_queue_capacity(&dir, "127.0.0.1:0", 1);
    let collector = Collector::spawn(&path);

    let body = build_test_batch(1, ALL_ZERO_TRACE_ID, "host-no-partial", 99, 1_111);
    let bound = collector.bound.clone();

    let mut handles = Vec::with_capacity(8);
    for _ in 0..8 {
        let bound = bound.clone();
        let body = body.clone();
        handles.push(std::thread::spawn(move || {
            let req = ingest_request(&bound, &body);
            send_raw(&bound, &req)
        }));
    }
    for h in handles {
        let _ = h.join().unwrap();
    }

    std::thread::sleep(Duration::from_millis(200));
    let partials = list_partials(&data_dir);
    assert!(
        partials.is_empty(),
        "expected no partials after backpressure storm; got {partials:?}"
    );
}

#[test]
fn backpressure_503_response_body_and_header() {
    let dir = unique_tempdir("backpressure_body");
    let path = write_config_with_queue_capacity(&dir, "127.0.0.1:0", 1);
    let collector = Collector::spawn(&path);

    // Use a raw TCP read so we can inspect the full response (header
    // block + body) rather than only the parsed body. We dispatch
    // a burst, find the first 503, and assert on its raw bytes.
    let body = build_test_batch(1, ALL_ZERO_TRACE_ID, "host-bphdr", 7, 99_999);
    let bound = collector.bound.clone();

    let mut handles = Vec::with_capacity(5);
    for _ in 0..5 {
        let bound = bound.clone();
        let body = body.clone();
        handles.push(std::thread::spawn(move || {
            // Re-implement send_raw inline so we can keep the raw
            // response (head + body together).
            let mut stream = TcpStream::connect(&bound).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let req = ingest_request(&bound, &body);
            stream.write_all(&req).unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            response
        }));
    }
    let raws: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    let raw_503 = raws
        .iter()
        .find(|r| r.starts_with("HTTP/1.1 503"))
        .unwrap_or_else(|| panic!("no 503 response among {raws:?}"));

    // Body is after the blank line separator.
    let (head, body_str) = raw_503
        .split_once("\r\n\r\n")
        .expect("response missing header/body delimiter");
    assert!(
        head.to_ascii_lowercase().contains("connection: close"),
        "503 must carry Connection: close; head was: {head}"
    );
    assert_eq!(body_str, r#"{"error":"backpressure"}"#);
}

// ============================================================
// Storage tests (added by the storage-sqlite change)
// ============================================================

/// Open `<data_dir>/index.sqlite` read-only for test inspection.
/// The collector itself writes to the same file via its own
/// connection; SQLite's WAL mode lets the test read concurrent
/// committed state.
fn open_index_db_ro(data_dir: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open_with_flags(
        data_dir.join("index.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("could not open index.sqlite read-only")
}

/// Open `<data_dir>/traces/<key>.sqlite` read-only.
fn open_trace_db_ro(data_dir: &Path, key: &str) -> rusqlite::Connection {
    let path = data_dir.join("traces").join(format!("{key}.sqlite"));
    rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap_or_else(|e| panic!("could not open {path:?}: {e}"))
}

#[test]
fn valid_v1_body_records_a_trace_row() {
    let dir = unique_tempdir("records_trace");
    let data_dir = dir.join("data");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    // The captured fixture: meta is fixed, dict.len()=2, calls.len()=10000.
    let req = ingest_request(&collector.bound, FIXTURE_FLAT_CALLS_1);
    let (status, _) = send_raw(&collector.bound, &req);
    assert_eq!(status, 200);

    // Let the decoder finish writing.
    collector.wait_for_stdout(EVENT_BATCH_ACCEPTED, Duration::from_secs(5));

    let conn = open_index_db_ro(&data_dir);
    let (trace_key, batch_count, call_count, state): (String, i64, i64, String) = conn
        .query_row(
            "SELECT trace_key, batch_count, call_count, state FROM traces",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .expect("expected one row in traces");
    assert_eq!(trace_key.len(), 32, "trace_key must be 32 hex chars");
    assert_eq!(batch_count, 1);
    assert_eq!(call_count, 10_000, "fixture has 10000 calls");
    assert_eq!(state, "active");
}

#[test]
fn consecutive_same_trace_batches_increment_counters_in_index() {
    let dir = unique_tempdir("increment_counters");
    let data_dir = dir.join("data");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let body = build_test_batch(1, ALL_ZERO_TRACE_ID, "host-counters", 99, 4_242_424_242);
    let calls_per_batch = 0; // build_test_batch ships empty calls/dict

    for _ in 0..3 {
        let req = ingest_request(&collector.bound, &body);
        let (status, _) = send_raw(&collector.bound, &req);
        assert_eq!(status, 200);
    }
    // Wait for all three to land. The decoder logs one
    // "decoded batch" line per submission; the third occurrence is
    // our quiescence signal.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let stdout = collector.stdout_so_far.lock().unwrap().clone();
        let count = stdout.matches(EVENT_BATCH_ACCEPTED).count();
        if count >= 3 {
            break;
        }
        if Instant::now() > deadline {
            panic!("only {count}/3 decoded-batch lines within 5s\nstdout:\n{stdout}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let conn = open_index_db_ro(&data_dir);
    let (batch_count, call_count, first_at, last_at): (i64, i64, i64, i64) = conn
        .query_row(
            "SELECT batch_count, call_count, first_batch_at_ns, last_batch_at_ns FROM traces",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(batch_count, 3);
    assert_eq!(call_count, 3 * calls_per_batch);
    assert!(
        last_at >= first_at,
        "last_at ({last_at}) must be >= first_at ({first_at})"
    );
}

#[test]
fn per_trace_sqlite_has_dict_after_first_batch() {
    let dir = unique_tempdir("dict_present");
    let data_dir = dir.join("data");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let req = ingest_request(&collector.bound, FIXTURE_FLAT_CALLS_1);
    let (status, _) = send_raw(&collector.bound, &req);
    assert_eq!(status, 200);
    collector.wait_for_stdout(EVENT_BATCH_ACCEPTED, Duration::from_secs(5));

    // Discover the trace key from the index DB (depends on fixture meta).
    let trace_key: String = open_index_db_ro(&data_dir)
        .query_row("SELECT trace_key FROM traces", [], |r| r.get(0))
        .unwrap();

    let trace_conn = open_trace_db_ro(&data_dir, &trace_key);
    // Filter out the synthetic root (fn_id=0) so we count only the
    // dict entries that came over the wire.
    let dict_count: i64 = trace_conn
        .query_row("SELECT COUNT(*) FROM dict WHERE fn_id > 0", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(dict_count, 2, "fixture's dict has 2 entries");

    // Spot-check that the schema actually populated the row content.
    let (fn_id, fqn): (i64, String) = trace_conn
        .query_row(
            "SELECT fn_id, fqn FROM dict WHERE fn_id > 0 ORDER BY fn_id LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(fn_id > 0);
    assert!(!fqn.is_empty(), "fqn must be populated");
}

#[test]
fn well_formed_fixture_produces_no_anomalies() {
    // The captured `flat_calls` fixture is well-formed (all Calls
    // carry a `fn_id` that's in dict, all have `t_in < t_out`), so
    // none of the DQ kinds fire. Sanity baseline so a regression
    // in the detection thresholds shows up here.
    let dir = unique_tempdir("anom_empty_e2e");
    let data_dir = dir.join("data");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let req = ingest_request(&collector.bound, FIXTURE_FLAT_CALLS_1);
    let (status, _) = send_raw(&collector.bound, &req);
    assert_eq!(status, 200);
    collector.wait_for_stdout(EVENT_BATCH_ACCEPTED, Duration::from_secs(5));

    let trace_key: String = open_index_db_ro(&data_dir)
        .query_row("SELECT trace_key FROM traces", [], |r| r.get(0))
        .unwrap();
    let conn = open_trace_db_ro(&data_dir, &trace_key);

    let n_anom: i64 = conn
        .query_row("SELECT COUNT(*) FROM anomalies", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_anom, 0, "well-formed fixture should produce no anomalies");

    let n_index: i64 = open_index_db_ro(&data_dir)
        .query_row("SELECT anomaly_count FROM traces", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_index, 0);
}

#[test]
fn dq1_batch_parks_call_and_writes_unresolved_fn_at_finalize_e2e() {
    // Two phases under one collector. First: the batch lands, the
    // Call parks in `pending_calls`, no anomaly row exists yet.
    // Second: the fast-finalize tick fires; the residual pending
    // row becomes a single `unresolved_fn` anomaly row.
    let dir = unique_tempdir("dq1_e2e");
    let data_dir = dir.join("data");
    let path = write_config_with_fast_finalize(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let body = build_test_batch_with_unresolved_fn("dq1-host", 1, 1);
    let req = ingest_request(&collector.bound, &body);
    let (status, _) = send_raw(&collector.bound, &req);
    assert_eq!(status, 200);
    collector.wait_for_stdout(EVENT_BATCH_ACCEPTED, Duration::from_secs(5));

    // ---- Phase 1: post-batch, pre-finalize ----
    let trace_key: String = open_index_db_ro(&data_dir)
        .query_row("SELECT trace_key FROM traces", [], |r| r.get(0))
        .unwrap();
    let conn = open_trace_db_ro(&data_dir, &trace_key);
    let n_anom_before: i64 = conn
        .query_row("SELECT COUNT(*) FROM anomalies", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        n_anom_before, 0,
        "DQ-1 must NOT be written during record_batch"
    );
    let pending_before: (i64, i64, i64) = conn
        .query_row(
            "SELECT call_id, parent_call_id, fn_id FROM pending_calls",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(pending_before, (1, 0, 99));

    // ---- Phase 2: post-finalize ----
    collector.wait_for_stdout(EVENT_TRACE_FINALIZED, Duration::from_secs(5));
    let conn = open_trace_db_ro(&data_dir, &trace_key);
    let (node_id, kind, sample_call_id, detail): (Option<i64>, String, i64, String) = conn
        .query_row(
            "SELECT node_id, kind, sample_call_id, detail FROM anomalies",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(node_id, None);
    assert_eq!(kind, "unresolved_fn");
    assert_eq!(sample_call_id, 1);
    assert_eq!(detail, "fn_id=99");

    let n_pending: i64 = conn
        .query_row("SELECT COUNT(*) FROM pending_calls", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_pending, 0, "pending_calls drained at finalize");

    let n_index: i64 = open_index_db_ro(&data_dir)
        .query_row("SELECT anomaly_count FROM traces", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_index, 1);
}

#[test]
fn dq1_no_longer_emits_stderr_line() {
    let dir = unique_tempdir("dq1_no_stderr");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let body = build_test_batch_with_unresolved_fn("dq1-quiet", 1, 1);
    let req = ingest_request(&collector.bound, &body);
    let (status, _) = send_raw(&collector.bound, &req);
    assert_eq!(status, 200);
    collector.wait_for_stdout(EVENT_BATCH_ACCEPTED, Duration::from_secs(5));

    let stderr = collector.stderr_so_far.lock().unwrap().clone();
    assert!(
        !stderr.contains("aggregate: dq1 skipped"),
        "DQ-1 stderr line should be gone now that anomalies records it; stderr:\n{stderr}"
    );
}

#[test]
fn decoded_batch_log_carries_anomalies_and_dict_pending_fields() {
    // Post `tolerate-out-of-order-batches`: a DQ-1 batch parks the
    // call rather than writing an anomaly during `record_batch`, so
    // the `anomalies` field on the `batch accepted` event is 0 for
    // this shape — the new `dict_pending` field records the parking
    // instead. Use an inverted-time batch to confirm DQ-3 still
    // increments `anomalies` in-batch.
    let dir = unique_tempdir("anom_log_field");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    // 1) Unknown-fn batch: anomalies=0, dict_pending=1.
    let body = build_test_batch_with_unresolved_fn("dict-pending-host", 1, 1);
    let req = ingest_request(&collector.bound, &body);
    let (status, _) = send_raw(&collector.bound, &req);
    assert_eq!(status, 200);
    collector.wait_for_stdout(EVENT_BATCH_ACCEPTED, Duration::from_secs(5));

    // 2) Inverted-time batch (different host → different trace_key):
    //    anomalies=1, dict_pending=0.
    let body = build_test_batch_with_inverted_time("dq3-log-host", 1, 1);
    let req = ingest_request(&collector.bound, &body);
    let (status, _) = send_raw(&collector.bound, &req);
    assert_eq!(status, 200);
    // Wait until BOTH batch-accepted lines have been written.
    let deadline = Instant::now() + Duration::from_secs(5);
    let stdout = loop {
        let s = collector.stdout_so_far.lock().unwrap().clone();
        if s.matches(EVENT_BATCH_ACCEPTED).count() >= 2 {
            break s;
        }
        if Instant::now() > deadline {
            panic!("only one batch-accepted line within 5s:\n{s}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    let lines: Vec<&str> = stdout
        .lines()
        .filter(|l| l.contains(EVENT_BATCH_ACCEPTED))
        .collect();
    assert_eq!(lines.len(), 2, "expected two batch-accepted lines");

    // Tracing's default fmt layer renders string fields unquoted
    // (`host=dict-pending-host`). Match on the bare host substring so
    // the assertion isn't tied to a specific quoting convention.
    let dict_pending_line = lines
        .iter()
        .find(|l| l.contains("dict-pending-host"))
        .expect("dict-pending-host line missing");
    assert!(
        dict_pending_line.contains("anomalies=0"),
        "expected anomalies=0 for parked-DQ-1 batch in: {dict_pending_line}"
    );
    assert!(
        dict_pending_line.contains("dict_pending=1"),
        "expected dict_pending=1 in: {dict_pending_line}"
    );

    let dq3_line = lines
        .iter()
        .find(|l| l.contains("dq3-log-host"))
        .expect("dq3-log-host line missing");
    assert!(
        dq3_line.contains("anomalies=1"),
        "expected anomalies=1 for DQ-3 batch in: {dq3_line}"
    );
    assert!(
        dq3_line.contains("dict_pending=0"),
        "expected dict_pending=0 for DQ-3 batch in: {dq3_line}"
    );
}

#[test]
fn dq3_e2e_anomaly_attaches_to_resulting_node() {
    let dir = unique_tempdir("dq3_e2e");
    let data_dir = dir.join("data");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let body = build_test_batch_with_inverted_time("dq3-host", 1, 1);
    let req = ingest_request(&collector.bound, &body);
    let (status, _) = send_raw(&collector.bound, &req);
    assert_eq!(status, 200);
    collector.wait_for_stdout(EVENT_BATCH_ACCEPTED, Duration::from_secs(5));

    let trace_key: String = open_index_db_ro(&data_dir)
        .query_row("SELECT trace_key FROM traces", [], |r| r.get(0))
        .unwrap();
    let conn = open_trace_db_ro(&data_dir, &trace_key);

    // SELECT JOIN to confirm the anomaly's node_id resolves to a
    // real `nodes` row with fn_id=7.
    let (kind, fn_id, total_wall): (String, i64, i64) = conn
        .query_row(
            "SELECT a.kind, n.fn_id, n.total_wall_ns \
             FROM anomalies a JOIN nodes n ON n.node_id = a.node_id",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(kind, "inverted_time");
    assert_eq!(fn_id, 7);
    assert_eq!(
        total_wall, 0,
        "the inverted wall clamps to 0; the row still folds"
    );
}

#[test]
fn anomaly_count_on_traces_row_mirrors_per_trace_anomalies_e2e() {
    let dir = unique_tempdir("anom_count_e2e");
    let data_dir = dir.join("data");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    // Two requests: one unknown-fn (DQ-1 deferred to finalize), one
    // DQ-3 (anomaly written in-batch), against different hosts so
    // they get distinct trace_keys. The invariant under test is the
    // in-batch shape:
    //     traces.anomaly_count == per-trace SELECT COUNT(*)
    // For the unknown-fn trace both are 0 (the call sits in
    // pending_calls); for the DQ-3 trace both are 1. The "DQ-1
    // eventually becomes 1" assertion lives in
    // `dq1_batch_parks_call_and_writes_unresolved_fn_at_finalize_e2e`.
    for (host, body) in [
        (
            "anom-e2e-a",
            build_test_batch_with_unresolved_fn("anom-e2e-a", 1, 1),
        ),
        (
            "anom-e2e-b",
            build_test_batch_with_inverted_time("anom-e2e-b", 1, 1),
        ),
    ] {
        let req = ingest_request(&collector.bound, &body);
        let (status, _) = send_raw(&collector.bound, &req);
        assert_eq!(status, 200, "host {host} request must 200");
    }
    // Wait for both decoded-batch lines.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let stdout = collector.stdout_so_far.lock().unwrap().clone();
        if stdout.matches(EVENT_BATCH_ACCEPTED).count() >= 2 {
            break;
        }
        if Instant::now() > deadline {
            panic!("only one decoded-batch line within 5s:\n{stdout}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let index = open_index_db_ro(&data_dir);
    let mut stmt = index
        .prepare("SELECT trace_key, anomaly_count FROM traces")
        .unwrap();
    let rows: Vec<(String, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(rows.len(), 2, "two distinct traces");
    for (trace_key, n_index) in rows {
        let conn = open_trace_db_ro(&data_dir, &trace_key);
        let n_per_trace: i64 = conn
            .query_row("SELECT COUNT(*) FROM anomalies", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            n_per_trace, n_index,
            "trace {trace_key}: index anomaly_count must match per-trace COUNT(*)"
        );
    }
}

#[test]
fn consecutive_batches_with_anomalies_accumulate_the_counter() {
    let dir = unique_tempdir("anom_accumulate");
    let data_dir = dir.join("data");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    // Two DQ-3 batches against the same host → same trace_key →
    // counter must sum. (Pre `tolerate-out-of-order-batches` this
    // test used DQ-1 batches; those no longer write anomalies in-
    // batch, so the accumulation invariant is exercised here with
    // DQ-3 — which still emits its anomaly row immediately.)
    for _ in 0..2 {
        let body = build_test_batch_with_inverted_time("anom-acc", 7, 7);
        let req = ingest_request(&collector.bound, &body);
        let (status, _) = send_raw(&collector.bound, &req);
        assert_eq!(status, 200);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let stdout = collector.stdout_so_far.lock().unwrap().clone();
        if stdout.matches(EVENT_BATCH_ACCEPTED).count() >= 2 {
            break;
        }
        if Instant::now() > deadline {
            panic!("only one decoded-batch line within 5s:\n{stdout}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let n_index: i64 = open_index_db_ro(&data_dir)
        .query_row("SELECT anomaly_count FROM traces", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_index, 2, "two batches, one anomaly each, total 2");

    let trace_key: String = open_index_db_ro(&data_dir)
        .query_row("SELECT trace_key FROM traces", [], |r| r.get(0))
        .unwrap();
    let conn = open_trace_db_ro(&data_dir, &trace_key);
    let n_per_trace: i64 = conn
        .query_row("SELECT COUNT(*) FROM anomalies", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_per_trace, 2);
}

#[test]
fn aggregated_nodes_populate_after_a_batch() {
    // The captured fixtures are mid-trace snapshots: every chain
    // roots on a call_id whose exit hasn't reached us, so nothing
    // aggregates from them alone. Use a synthetic batch with a real
    // `parent=0` top-level call so the aggregation path produces
    // observable rows.
    let dir = unique_tempdir("nodes_populate");
    let data_dir = dir.join("data");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let body = build_test_batch_with_chain("host-agg", 100, 1_000_001);
    let req = ingest_request(&collector.bound, &body);
    let (status, _) = send_raw(&collector.bound, &req);
    assert_eq!(status, 200);
    collector.wait_for_stdout(EVENT_BATCH_ACCEPTED, Duration::from_secs(5));

    let trace_key: String = open_index_db_ro(&data_dir)
        .query_row("SELECT trace_key FROM traces", [], |r| r.get(0))
        .unwrap();
    let conn = open_trace_db_ro(&data_dir, &trace_key);

    // Synthetic root + the two user-call nodes from the chain.
    let n_nodes: i64 = conn
        .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        n_nodes, 3,
        "expected synthetic root + 2 user nodes; got {n_nodes}"
    );

    // The top-level call (fn_id=1) sits under root; the child
    // (fn_id=2) sits under the top-level node.
    let top_self: i64 = conn
        .query_row("SELECT total_wall_ns FROM nodes WHERE fn_id = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    let top_children: i64 = conn
        .query_row(
            "SELECT children_total_wall_ns FROM nodes WHERE fn_id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(top_self, 200);
    assert_eq!(top_children, 50, "child contributed its 50ns wall");

    // For every non-root node, children_total_wall_ns ≤
    // total_wall_ns (self_wall non-negative).
    let mut stmt = conn
        .prepare("SELECT total_wall_ns, children_total_wall_ns FROM nodes WHERE node_id > 1")
        .unwrap();
    let rows: Vec<(i64, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    for (total, children) in rows {
        assert!(
            total >= children,
            "self_wall must be non-negative: total={total} children={children}"
        );
    }
}

#[test]
fn synthetic_root_exists_post_aggregation() {
    let dir = unique_tempdir("synth_root_e2e");
    let data_dir = dir.join("data");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let req = ingest_request(&collector.bound, FIXTURE_FLAT_CALLS_1);
    let (status, _) = send_raw(&collector.bound, &req);
    assert_eq!(status, 200);
    collector.wait_for_stdout(EVENT_BATCH_ACCEPTED, Duration::from_secs(5));

    let trace_key: String = open_index_db_ro(&data_dir)
        .query_row("SELECT trace_key FROM traces", [], |r| r.get(0))
        .unwrap();
    let conn = open_trace_db_ro(&data_dir, &trace_key);

    let (node_id, fn_id): (i64, i64) = conn
        .query_row(
            "SELECT node_id, fn_id FROM nodes WHERE parent_node_id IS NULL",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(node_id, 1);
    assert_eq!(fn_id, 0);

    // Dict carries the <root> entry too.
    let fqn: String = conn
        .query_row("SELECT fqn FROM dict WHERE fn_id = 0", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fqn, "<root>");
}

#[test]
fn call_to_node_populates_e2e() {
    // Use the synthetic chain: captured fixtures aren't usable —
    // every chain in them roots on an unseen call_id, so nothing
    // ever lands in `call_to_node`.
    let dir = unique_tempdir("c2n_populate");
    let data_dir = dir.join("data");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let body = build_test_batch_with_chain("host-c2n", 200, 1_000_002);
    let req = ingest_request(&collector.bound, &body);
    let (status, _) = send_raw(&collector.bound, &req);
    assert_eq!(status, 200);
    collector.wait_for_stdout(EVENT_BATCH_ACCEPTED, Duration::from_secs(5));

    let trace_key: String = open_index_db_ro(&data_dir)
        .query_row("SELECT trace_key FROM traces", [], |r| r.get(0))
        .unwrap();
    let conn = open_trace_db_ro(&data_dir, &trace_key);

    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM call_to_node", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2, "one mapping per Call in the chain (top + child)");
}

#[test]
fn decoded_batch_log_carries_nodes_and_pending_fields() {
    let dir = unique_tempdir("log_nodes_pending");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let req = ingest_request(&collector.bound, FIXTURE_FLAT_CALLS_1);
    let (status, _) = send_raw(&collector.bound, &req);
    assert_eq!(status, 200);
    let stdout = collector.wait_for_stdout(EVENT_BATCH_ACCEPTED, Duration::from_secs(5));

    let line = stdout
        .lines()
        .find(|l| l.contains(EVENT_BATCH_ACCEPTED))
        .unwrap_or_else(|| panic!("no decoded batch line in stdout: {stdout:?}"));
    assert!(line.contains("nodes="), "missing nodes= field in: {line}");
    assert!(
        line.contains("pending="),
        "missing pending= field in: {line}"
    );
}

#[test]
fn per_trace_dict_is_idempotent_across_batches_e2e() {
    let dir = unique_tempdir("dict_idemp_e2e");
    let data_dir = dir.join("data");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    // Send the same fixture twice for the same synthesized trace.
    for _ in 0..2 {
        let req = ingest_request(&collector.bound, FIXTURE_FLAT_CALLS_1);
        let (status, _) = send_raw(&collector.bound, &req);
        assert_eq!(status, 200);
    }
    // Wait for both submissions to finish recording.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let stdout = collector.stdout_so_far.lock().unwrap().clone();
        if stdout.matches(EVENT_BATCH_ACCEPTED).count() >= 2 {
            break;
        }
        if Instant::now() > deadline {
            panic!("only one decoded-batch line within 5s\nstdout:\n{stdout}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let trace_key: String = open_index_db_ro(&data_dir)
        .query_row("SELECT trace_key FROM traces", [], |r| r.get(0))
        .unwrap();
    let conn = open_trace_db_ro(&data_dir, &trace_key);

    // Filter out the synthetic root (fn_id=0).
    let dict_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM dict WHERE fn_id > 0", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(dict_count, 2, "dict should not duplicate across batches");
}

#[test]
fn pragma_user_version_is_1_on_both_dbs() {
    let dir = unique_tempdir("user_version_e2e");
    let data_dir = dir.join("data");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let req = ingest_request(&collector.bound, FIXTURE_FLAT_CALLS_1);
    let (status, _) = send_raw(&collector.bound, &req);
    assert_eq!(status, 200);
    collector.wait_for_stdout(EVENT_BATCH_ACCEPTED, Duration::from_secs(5));

    let index_conn = open_index_db_ro(&data_dir);
    let v: i64 = index_conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(v, 1, "index.sqlite user_version must be 1");

    let trace_key: String = index_conn
        .query_row("SELECT trace_key FROM traces", [], |r| r.get(0))
        .unwrap();
    let trace_conn = open_trace_db_ro(&data_dir, &trace_key);
    let v: i64 = trace_conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(v, 1, "per-trace .sqlite user_version must be 1");
}

#[test]
fn trace_meta_mirrors_the_index_row_e2e() {
    let dir = unique_tempdir("trace_meta_e2e");
    let data_dir = dir.join("data");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let req = ingest_request(&collector.bound, FIXTURE_FLAT_CALLS_1);
    let (status, _) = send_raw(&collector.bound, &req);
    assert_eq!(status, 200);
    collector.wait_for_stdout(EVENT_BATCH_ACCEPTED, Duration::from_secs(5));

    let index_conn = open_index_db_ro(&data_dir);
    let (key, host, pid): (String, String, i64) = index_conn
        .query_row("SELECT trace_key, host, pid FROM traces", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();

    let trace_conn = open_trace_db_ro(&data_dir, &key);
    let (mkey, mhost, mpid, mstate): (String, String, i64, String) = trace_conn
        .query_row(
            "SELECT trace_key, host, pid, state FROM trace_meta",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(mkey, key);
    assert_eq!(mhost, host);
    assert_eq!(mpid, pid);
    assert_eq!(mstate, "active");
}

// ---- idle-finalize ----

/// Count how many times the substring appears in `haystack`. Used by
/// the multi-trace finalize test to wait until N `finalized trace …`
/// lines have been emitted.
fn count_matches(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

/// Block until `predicate` returns true against fresh stdout, or panic
/// with a diagnostic dump once `timeout` elapses. Used when the test
/// needs to wait for *N* occurrences of a substring rather than just
/// one.
fn wait_until<P: Fn(&str) -> bool>(collector: &Collector, predicate: P, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let stdout = collector.stdout_so_far.lock().unwrap().clone();
        if predicate(&stdout) {
            return;
        }
        if Instant::now() > deadline {
            let stderr = collector.stderr_so_far.lock().unwrap().clone();
            panic!("timed out after {timeout:?}\nstdout:\n{stdout}\nstderr:\n{stderr}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn idle_finalize_marks_active_trace_as_finalized() {
    let dir = unique_tempdir("idle_finalize_basic");
    let data_dir = dir.join("data");
    let path = write_config_with_fast_finalize(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let body = build_test_batch_with_chain("finalize-basic", 1, 1);
    let req = ingest_request(&collector.bound, &body);
    let (status, _) = send_raw(&collector.bound, &req);
    assert_eq!(status, 200);
    collector.wait_for_stdout(EVENT_BATCH_ACCEPTED, Duration::from_secs(5));
    collector.wait_for_stdout(EVENT_TRACE_FINALIZED, Duration::from_secs(5));

    let state: String = open_index_db_ro(&data_dir)
        .query_row("SELECT state FROM traces", [], |r| r.get(0))
        .unwrap();
    assert_eq!(state, "finalized");
}

#[test]
fn idle_finalize_writes_dq2_anomaly_for_pending_parent() {
    let dir = unique_tempdir("idle_finalize_dq2");
    let data_dir = dir.join("data");
    let path = write_config_with_fast_finalize(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let body = build_test_batch_with_orphan_pending("dq2-host", 1, 1);
    let req = ingest_request(&collector.bound, &body);
    let (status, _) = send_raw(&collector.bound, &req);
    assert_eq!(status, 200);
    collector.wait_for_stdout(EVENT_BATCH_ACCEPTED, Duration::from_secs(5));
    collector.wait_for_stdout(EVENT_TRACE_FINALIZED, Duration::from_secs(5));

    let trace_key: String = open_index_db_ro(&data_dir)
        .query_row("SELECT trace_key FROM traces", [], |r| r.get(0))
        .unwrap();
    let conn = open_trace_db_ro(&data_dir, &trace_key);

    let n_pending: i64 = conn
        .query_row("SELECT COUNT(*) FROM pending_calls", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_pending, 0, "pending_calls drained at finalize");

    let (node_id, kind, sample_call_id, detail): (Option<i64>, String, i64, String) = conn
        .query_row(
            "SELECT node_id, kind, sample_call_id, detail FROM anomalies",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(node_id, None);
    assert_eq!(kind, "pending_parent_at_finalize");
    assert_eq!(sample_call_id, 42);
    assert_eq!(detail, "parent_call_id=999");

    let anomaly_count: i64 = open_index_db_ro(&data_dir)
        .query_row("SELECT anomaly_count FROM traces", [], |r| r.get(0))
        .unwrap();
    assert_eq!(anomaly_count, 1);
}

#[test]
fn idle_finalize_log_line_carries_pending_dq2_and_cpu_fields() {
    let dir = unique_tempdir("idle_finalize_log");
    let path = write_config_with_fast_finalize(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let body = build_test_batch_with_orphan_pending("dq2-log", 1, 1);
    let req = ingest_request(&collector.bound, &body);
    let (status, _) = send_raw(&collector.bound, &req);
    assert_eq!(status, 200);
    let stdout = collector.wait_for_stdout(EVENT_TRACE_FINALIZED, Duration::from_secs(5));

    let line = stdout
        .lines()
        .find(|l| l.contains(EVENT_TRACE_FINALIZED))
        .expect("trace finalized event must be present");
    assert!(
        line.contains("pending_dq2=1"),
        "expected pending_dq2=1 in: {line}"
    );
    // The orphan Call never folded into a node, so no node exists
    // with non-zero CPU → cpu_snapshot_available=false. (Tracing's
    // fmt layer renders booleans as `true`/`false`.)
    assert!(
        line.contains("cpu_snapshot_available=false"),
        "expected cpu_snapshot_available=false in: {line}"
    );
}

#[test]
fn idle_finalize_sets_cpu_snapshot_available_to_0_for_zero_cpu_trace() {
    let dir = unique_tempdir("idle_finalize_cpu_off");
    let data_dir = dir.join("data");
    let path = write_config_with_fast_finalize(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    // build_test_batch_with_inverted_time has cpu_u=0, cpu_s=0 on its
    // only Call, but it DOES fold into a node (with total_wall=0). So
    // the trace has one user node with zero CPU → cpu_snapshot_available
    // should be 0.
    let body = build_test_batch_with_inverted_time("cpu-off-trace", 1, 1);
    let req = ingest_request(&collector.bound, &body);
    let (status, _) = send_raw(&collector.bound, &req);
    assert_eq!(status, 200);
    collector.wait_for_stdout(EVENT_TRACE_FINALIZED, Duration::from_secs(5));

    let (cpu_index, trace_key): (i64, String) = open_index_db_ro(&data_dir)
        .query_row(
            "SELECT cpu_snapshot_available, trace_key FROM traces",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(cpu_index, 0);

    let cpu_meta: i64 = open_trace_db_ro(&data_dir, &trace_key)
        .query_row("SELECT cpu_snapshot_available FROM trace_meta", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(cpu_meta, 0);
}

#[test]
fn late_batch_after_finalize_reactivates_state_e2e() {
    let dir = unique_tempdir("late_batch_reactivate");
    let data_dir = dir.join("data");
    let path = write_config_with_fast_finalize(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    // Batch A.
    let body_a = build_test_batch_with_chain("late-host", 7, 7);
    let req_a = ingest_request(&collector.bound, &body_a);
    let (status, _) = send_raw(&collector.bound, &req_a);
    assert_eq!(status, 200);
    collector.wait_for_stdout(EVENT_TRACE_FINALIZED, Duration::from_secs(5));

    let state_after_a: String = open_index_db_ro(&data_dir)
        .query_row("SELECT state FROM traces", [], |r| r.get(0))
        .unwrap();
    assert_eq!(state_after_a, "finalized");

    // Batch B for the same trace (same host/pid/start_time → same
    // synthesized TraceKey).
    let body_b = build_test_batch_with_chain("late-host", 7, 7);
    let req_b = ingest_request(&collector.bound, &body_b);
    let (status, _) = send_raw(&collector.bound, &req_b);
    assert_eq!(status, 200);

    // Wait for the second `decoded batch …` line. The first batch
    // already produced one, so we wait for the count to reach 2.
    wait_until(
        &collector,
        |stdout| count_matches(stdout, EVENT_BATCH_ACCEPTED) >= 2,
        Duration::from_secs(5),
    );

    let (state_after_b, batch_count): (String, i64) = open_index_db_ro(&data_dir)
        .query_row("SELECT state, batch_count FROM traces", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(state_after_b, "active", "late batch reactivates the trace");
    assert_eq!(batch_count, 2);

    // trace_meta mirrors the index.
    let trace_key: String = open_index_db_ro(&data_dir)
        .query_row("SELECT trace_key FROM traces", [], |r| r.get(0))
        .unwrap();
    let mirror_state: String = open_trace_db_ro(&data_dir, &trace_key)
        .query_row("SELECT state FROM trace_meta", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mirror_state, "active");
}

#[test]
fn idle_finalize_does_not_finalize_traces_inside_threshold() {
    // Build a config with idle_seconds=60 and tick_seconds=1 — ticks
    // run, but the trace is too fresh to be finalized.
    let dir = unique_tempdir("idle_finalize_threshold");
    let data_dir = dir.join("data");
    let auto_data = dir.join("data");
    std::fs::create_dir_all(&auto_data).unwrap();
    let body = format!(
        r#"[server]
bind = "127.0.0.1:0"

[auth]
token = "{TOKEN}"
session_salt = "{SALT}"

[storage]
data_dir = "{}"
retention_days = 30

[finalize]
idle_seconds = 60
tick_seconds = 1
{TEXT_LOG_SECTION}"#,
        auto_data.display(),
    );
    let path = dir.join("collector.toml");
    std::fs::write(&path, body).unwrap();
    let collector = Collector::spawn(&path);

    let req = ingest_request(
        &collector.bound,
        &build_test_batch_with_chain("threshold-host", 1, 1),
    );
    let (status, _) = send_raw(&collector.bound, &req);
    assert_eq!(status, 200);
    collector.wait_for_stdout(EVENT_BATCH_ACCEPTED, Duration::from_secs(5));

    // Wait long enough for several finalize ticks, then assert state
    // is still active and no finalize line was emitted.
    std::thread::sleep(Duration::from_secs(3));
    let stdout = collector.stdout_so_far.lock().unwrap().clone();
    assert!(
        !stdout.contains(EVENT_TRACE_FINALIZED),
        "trace inside threshold must not be finalized; stdout:\n{stdout}"
    );

    let state: String = open_index_db_ro(&data_dir)
        .query_row("SELECT state FROM traces", [], |r| r.get(0))
        .unwrap();
    assert_eq!(state, "active");
}

#[test]
fn idle_finalize_log_is_silent_when_nothing_idle() {
    let dir = unique_tempdir("idle_finalize_quiet");
    let path = write_config_with_fast_finalize(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    // No batches sent. Wait through several finalize ticks.
    std::thread::sleep(Duration::from_secs(3));

    let stdout = collector.stdout_so_far.lock().unwrap().clone();
    assert!(
        !stdout.contains(EVENT_TRACE_FINALIZED),
        "quiet collector must not emit finalize lines; stdout:\n{stdout}"
    );
}

#[test]
fn three_synthetic_traces_finalize_independently() {
    // Acceptance against §10.2 / S-1: "After replaying the three
    // handover workloads, `index.sqlite` has 3 rows with `state =
    // 'finalized'`". The captured handover fixtures are mid-trace
    // snapshots (per `COMMENTS.md`) and only batch-0001 of one of the
    // three workloads is included as an embedded fixture in this
    // crate today. We replicate the *acceptance* (three distinct
    // traces all finalize cleanly) with three synthetic batches built
    // from `build_test_batch_with_chain` — different (host, pid,
    // start_time) → different synthesized TraceKey → three rows in
    // `index.sqlite.traces`. Adding the literal handover replay is
    // tracked as a follow-up: it requires copying the remaining 8
    // .msgpack files into `tests/fixtures/`.
    let dir = unique_tempdir("three_traces");
    let data_dir = dir.join("data");
    let path = write_config_with_fast_finalize(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    for (host, pid, start) in [
        ("trace-one", 1, 100),
        ("trace-two", 2, 200),
        ("trace-three", 3, 300),
    ] {
        let req = ingest_request(
            &collector.bound,
            &build_test_batch_with_chain(host, pid, start),
        );
        let (status, _) = send_raw(&collector.bound, &req);
        assert_eq!(status, 200);
    }
    wait_until(
        &collector,
        |stdout| count_matches(stdout, EVENT_BATCH_ACCEPTED) >= 3,
        Duration::from_secs(5),
    );
    wait_until(
        &collector,
        |stdout| count_matches(stdout, EVENT_TRACE_FINALIZED) >= 3,
        Duration::from_secs(5),
    );

    let n_finalized: i64 = open_index_db_ro(&data_dir)
        .query_row(
            "SELECT COUNT(*) FROM traces WHERE state = 'finalized'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n_finalized, 3);
}

// ---- retention-sweeper ----

/// Current `CLOCK_REALTIME` in nanoseconds — same source the collector
/// uses for the retention cutoff. Used by the retention tests to
/// pin a batch's `meta.start_time` well past the cutoff.
fn now_realtime_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

const ONE_DAY_NS: i64 = 86_400 * 1_000_000_000;

#[test]
fn retention_sweeper_removes_expired_trace_files_and_row() {
    let dir = unique_tempdir("retention_basic");
    let data_dir = dir.join("data");
    let path = write_config_with_fast_retention(&dir, "127.0.0.1:0", 1);
    let collector = Collector::spawn(&path);

    // Two days ago = comfortably past the 1-day cutoff.
    let start_time = now_realtime_ns() - 2 * ONE_DAY_NS;
    let body = build_test_batch_with_chain("ret-basic", 1, start_time);
    let req = ingest_request(&collector.bound, &body);
    let (status, _) = send_raw(&collector.bound, &req);
    assert_eq!(status, 200);
    // Wait for the decoder to commit the trace, then for the sweeper
    // to remove it.
    collector.wait_for_stdout(EVENT_BATCH_ACCEPTED, Duration::from_secs(5));
    collector.wait_for_stdout(EVENT_RETENTION_SWEPT, Duration::from_secs(5));

    let trace_key = synth_trace_key("ret-basic", 1, start_time);
    let sqlite_path = data_dir.join("traces").join(format!("{trace_key}.sqlite"));
    assert!(!sqlite_path.exists(), "per-trace SQLite must be gone");

    let n_rows: i64 = open_index_db_ro(&data_dir)
        .query_row("SELECT COUNT(*) FROM traces", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_rows, 0, "index row must be gone");
}

#[test]
fn retention_sweeper_logs_summary_line_with_freed_bytes() {
    let dir = unique_tempdir("retention_log");
    let path = write_config_with_fast_retention(&dir, "127.0.0.1:0", 1);
    let collector = Collector::spawn(&path);

    let start_time = now_realtime_ns() - 2 * ONE_DAY_NS;
    let body = build_test_batch_with_chain("ret-log", 1, start_time);
    let req = ingest_request(&collector.bound, &body);
    let (status, _) = send_raw(&collector.bound, &req);
    assert_eq!(status, 200);
    let stdout = collector.wait_for_stdout(EVENT_RETENTION_SWEPT, Duration::from_secs(5));

    let line = stdout
        .lines()
        .find(|l| l.contains(EVENT_RETENTION_SWEPT))
        .expect("retention swept event must be present");
    assert!(
        line.contains("removed_traces=1"),
        "expected removed_traces=1 in: {line}"
    );
    // Parse `freed_bytes=<N>` and assert N > 0 (the per-trace SQLite
    // is always at least a few KB after `record_batch`).
    let freed: u64 = line
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("freed_bytes="))
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("could not parse freed_bytes from: {line}"));
    assert!(freed > 0, "freed_bytes must be > 0, got {freed}");
}

#[test]
fn retention_sweeper_silent_when_nothing_expired() {
    // Generous retention window — a freshly-ingested batch shouldn't
    // be touched. Sleep through a few ticks and assert no
    // `swept retention` line appeared.
    let dir = unique_tempdir("retention_silent");
    let path = write_config_with_fast_retention(&dir, "127.0.0.1:0", 30);
    let collector = Collector::spawn(&path);

    // A "current" trace — start_time only one ms ago.
    let start_time = now_realtime_ns() - 1_000_000;
    let body = build_test_batch_with_chain("ret-silent", 1, start_time);
    let req = ingest_request(&collector.bound, &body);
    let (status, _) = send_raw(&collector.bound, &req);
    assert_eq!(status, 200);
    collector.wait_for_stdout(EVENT_BATCH_ACCEPTED, Duration::from_secs(5));

    // Let the retention loop tick a few times (tick_seconds = 1).
    std::thread::sleep(Duration::from_secs(3));

    let stdout = collector.stdout_so_far.lock().unwrap().clone();
    assert!(
        !stdout.contains(EVENT_RETENTION_SWEPT),
        "fresh trace must not trigger a sweep; stdout:\n{stdout}"
    );
}

#[test]
fn retention_sweeper_does_not_disturb_fresh_traces() {
    let dir = unique_tempdir("retention_mixed");
    let data_dir = dir.join("data");
    let path = write_config_with_fast_retention(&dir, "127.0.0.1:0", 1);
    let collector = Collector::spawn(&path);

    // Expired batch.
    let expired_start = now_realtime_ns() - 2 * ONE_DAY_NS;
    let body_a = build_test_batch_with_chain("ret-old", 1, expired_start);
    let (status, _) = send_raw(&collector.bound, &ingest_request(&collector.bound, &body_a));
    assert_eq!(status, 200);

    // Fresh batch (different host so it gets its own trace_key).
    let fresh_start = now_realtime_ns() - 1_000_000;
    let body_b = build_test_batch_with_chain("ret-new", 2, fresh_start);
    let (status, _) = send_raw(&collector.bound, &ingest_request(&collector.bound, &body_b));
    assert_eq!(status, 200);

    // Wait for both decodes, then for the retention summary that
    // covers the expired one.
    wait_until(
        &collector,
        |stdout| count_matches(stdout, EVENT_BATCH_ACCEPTED) >= 2,
        Duration::from_secs(5),
    );
    collector.wait_for_stdout(EVENT_RETENTION_SWEPT, Duration::from_secs(5));

    // The fresh trace must still be present; the expired one gone.
    let (n_rows, sole_key): (i64, String) = open_index_db_ro(&data_dir)
        .query_row(
            "SELECT COUNT(*), COALESCE(MAX(trace_key), '') FROM traces",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(n_rows, 1, "exactly one trace should survive");
    let fresh_key = synth_trace_key("ret-new", 2, fresh_start);
    assert_eq!(sole_key, fresh_key, "the surviving trace is the fresh one");
}
