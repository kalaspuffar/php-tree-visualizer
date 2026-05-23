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
"#
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

        // Read stdout up to the "listening on" line synchronously so
        // we know the bind succeeded and can learn the port. After
        // that, hand the rest off to a background drainer.
        let mut reader = BufReader::new(stdout);
        let deadline = Instant::now() + Duration::from_secs(10);
        let bound = loop {
            if Instant::now() > deadline {
                let captured_err = stderr_buf.lock().unwrap().clone();
                let captured_out = stdout_buf.lock().unwrap().clone();
                let _ = child.kill();
                panic!(
                    "binary did not announce 'listening on …' within 10s\nstdout so far:\n{captured_out}\nstderr so far:\n{captured_err}",
                );
            }
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    // Process exited before announcing — surface stderr.
                    let captured_err = stderr_buf.lock().unwrap().clone();
                    let captured_out = stdout_buf.lock().unwrap().clone();
                    let _ = child.wait();
                    panic!(
                        "binary exited before announcing\nstdout:\n{captured_out}\nstderr:\n{captured_err}",
                    );
                }
                Ok(_) => {
                    stdout_buf.lock().unwrap().push_str(&line);
                    if let Some(rest) = line.trim().strip_prefix("listening on ") {
                        break rest.to_owned();
                    }
                }
                Err(e) => panic!("could not read stdout: {e}"),
            }
        };

        // Drain the remainder of stdout in the background.
        {
            let buf = stdout_buf.clone();
            std::thread::spawn(move || {
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

        Self {
            child: Some(child),
            bound,
            stdout_so_far: stdout_buf,
            stderr_so_far: stderr_buf,
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
    // already saw `listening on …`, but `loaded config from …` is the
    // line *before* it.
    std::thread::sleep(Duration::from_millis(50));
    let stdout = collector.stdout_so_far.lock().unwrap().clone();

    assert!(
        stdout.contains("loaded config from"),
        "missing 'loaded config from' in stdout: {stdout:?}"
    );
    assert!(
        stdout.contains("token=***"),
        "missing redaction marker in stdout: {stdout:?}"
    );
    assert!(
        !stdout.contains(TOKEN),
        "banner leaked the token: {stdout:?}"
    );
    assert!(!stdout.contains(SALT), "banner leaked the salt: {stdout:?}");
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
        .replace("/var/lib/php-tree-viz", data_dir.to_str().unwrap());

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
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains(blocker.to_str().unwrap()),
        "stderr: {stderr:?}"
    );
    assert!(stderr.contains("tmp"), "stderr should name tmp: {stderr:?}");
    assert_eq!(stderr.lines().count(), 1);
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
"#,
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
    let stderr = collector.stderr_so_far.lock().unwrap().clone();

    // Stderr should mention the parse failure with the broken
    // trace's key.
    let broken_key = synth_trace_key("host-broken", 11, 1_111_111);
    assert!(
        stderr
            .lines()
            .any(|l| l.starts_with("decoder: parse failed") && l.contains(&broken_key)),
        "expected a `decoder: parse failed` stderr line for the broken trace; \
         stderr was:\n{stderr}",
    );

    // Stdout should have a `decoded batch` line for the second
    // (well-formed) request, proving the decoder kept running.
    let good_key = synth_trace_key("host-good", 12, 2_222_222);
    assert!(
        stdout
            .lines()
            .any(|l| l.starts_with("decoded batch") && l.contains(&good_key)),
        "expected a `decoded batch` line for the good trace after the broken one; \
         stdout was:\n{stdout}",
    );

    // And no `decoded batch` line for the broken trace.
    assert!(
        !stdout
            .lines()
            .any(|l| l.starts_with("decoded batch") && l.contains(&broken_key)),
        "decoded line for the broken trace shouldn't exist; stdout: {stdout}",
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
        .find(|l| l.starts_with("decoded batch") && l.contains(&expected_field))
        .unwrap_or_else(|| {
            panic!("no matching decoded line for {expected_field} in stdout: {stdout:?}")
        });
    assert!(line.contains(".raw/batch-0001.msgpack"));
    // `build_test_batch` ships `dict = []` and `calls = []`, so the
    // log line should show counts of 0 for both. This anchors the
    // test against the format, not against a specific workload.
    assert!(line.contains("dict=0"), "missing dict=0 in: {line}");
    assert!(line.contains("calls=0"), "missing calls=0 in: {line}");
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
    std::thread::sleep(Duration::from_millis(250));

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
    std::thread::sleep(Duration::from_millis(250));

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
    std::thread::sleep(Duration::from_millis(250));

    // Discover the trace key from the index DB (depends on fixture meta).
    let trace_key: String = open_index_db_ro(&data_dir)
        .query_row("SELECT trace_key FROM traces", [], |r| r.get(0))
        .unwrap();

    let trace_conn = open_trace_db_ro(&data_dir, &trace_key);
    let dict_count: i64 = trace_conn
        .query_row("SELECT COUNT(*) FROM dict", [], |r| r.get(0))
        .unwrap();
    assert_eq!(dict_count, 2, "fixture's dict has 2 entries");

    // Spot-check that the schema actually populated the row content.
    let (fn_id, fqn): (i64, String) = trace_conn
        .query_row(
            "SELECT fn_id, fqn FROM dict ORDER BY fn_id LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(fn_id > 0);
    assert!(!fqn.is_empty(), "fqn must be populated");
}

#[test]
fn aggregation_tables_stay_empty_after_recording() {
    let dir = unique_tempdir("agg_empty_e2e");
    let data_dir = dir.join("data");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let req = ingest_request(&collector.bound, FIXTURE_FLAT_CALLS_1);
    let (status, _) = send_raw(&collector.bound, &req);
    assert_eq!(status, 200);
    std::thread::sleep(Duration::from_millis(250));

    let trace_key: String = open_index_db_ro(&data_dir)
        .query_row("SELECT trace_key FROM traces", [], |r| r.get(0))
        .unwrap();
    let conn = open_trace_db_ro(&data_dir, &trace_key);

    for table in ["nodes", "call_to_node", "pending_calls", "anomalies"] {
        let n: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "{table} should be empty until the aggregation slice");
    }
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
    std::thread::sleep(Duration::from_millis(250));

    let trace_key: String = open_index_db_ro(&data_dir)
        .query_row("SELECT trace_key FROM traces", [], |r| r.get(0))
        .unwrap();
    let conn = open_trace_db_ro(&data_dir, &trace_key);

    let dict_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM dict", [], |r| r.get(0))
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
    std::thread::sleep(Duration::from_millis(250));

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
    std::thread::sleep(Duration::from_millis(250));

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
