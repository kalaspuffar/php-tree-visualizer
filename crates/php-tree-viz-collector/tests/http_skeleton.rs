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

#[test]
fn valid_request_returns_501_placeholder() {
    let dir = unique_tempdir("placeholder");
    let path = write_config(&dir, "127.0.0.1:0");
    let collector = Collector::spawn(&path);

    let req = request(
        "POST",
        "/ingest/v1",
        &[
            ("Authorization", &format!("Bearer {TOKEN}")),
            ("Content-Type", MEDIA_TYPE),
        ],
        "",
        &collector.bound,
    );
    let (status, body) = send_raw(&collector.bound, &req);
    assert_eq!(status, 501);
    assert!(body.contains("not_yet_implemented"));
    assert!(body.contains("body handling lands in the next change"));
}

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

#[test]
fn within_cap_body_lands_on_disk_as_partial() {
    let dir = unique_tempdir("within_cap");
    let path = write_config_with_overrides(
        &dir,
        "127.0.0.1:0",
        dir.join("data").to_str().unwrap(),
        Some(10_240),
    );
    let data_dir = dir.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let collector = Collector::spawn(&path);

    let body = vec![b'A'; 1024];
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
    assert_eq!(status, 501);
    assert!(resp_body.contains("not_yet_implemented"));

    // Allow the handler to flush + close. Then verify exactly one
    // partial file of length 1024 exists.
    std::thread::sleep(Duration::from_millis(100));
    let partials = list_partials(&data_dir);
    assert_eq!(
        partials.len(),
        1,
        "expected 1 partial file, got {partials:?}"
    );
    let size = std::fs::metadata(&partials[0]).unwrap().len();
    assert_eq!(size, 1024, "partial file size mismatch");
}

#[test]
fn concurrent_requests_produce_distinct_partial_files() {
    let dir = unique_tempdir("concurrent_partials");
    let path = write_config_with_overrides(
        &dir,
        "127.0.0.1:0",
        dir.join("data").to_str().unwrap(),
        Some(10_240),
    );
    let data_dir = dir.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let collector = Collector::spawn(&path);

    let bound = collector.bound.clone();
    let mut handles = Vec::with_capacity(8);
    for i in 0..8u8 {
        let bound = bound.clone();
        handles.push(std::thread::spawn(move || {
            let body = vec![i; 64];
            let req = request_with_body(
                "POST",
                "/ingest/v1",
                &[
                    ("Authorization", &format!("Bearer {TOKEN}")),
                    ("Content-Type", MEDIA_TYPE),
                ],
                &body,
                &bound,
            );
            send_raw(&bound, &req)
        }));
    }
    for h in handles {
        let (status, _) = h.join().unwrap();
        assert_eq!(status, 501);
    }

    std::thread::sleep(Duration::from_millis(150));
    let partials = list_partials(&data_dir);
    assert_eq!(
        partials.len(),
        8,
        "expected 8 distinct partial files, got {partials:?}"
    );
    let mut names: Vec<_> = partials
        .iter()
        .map(|p| p.file_name().unwrap().to_owned())
        .collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), 8, "filenames are not distinct");
}

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

#[test]
fn log_line_includes_body_bytes_for_within_cap_request() {
    let dir = unique_tempdir("log_body_bytes_ok");
    let path = write_config_with_overrides(
        &dir,
        "127.0.0.1:0",
        dir.join("data").to_str().unwrap(),
        Some(10_240),
    );
    std::fs::create_dir_all(dir.join("data")).unwrap();
    let collector = Collector::spawn(&path);

    let body = vec![b'A'; 1024];
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
    let (status, _) = send_raw(&collector.bound, &req);
    assert_eq!(status, 501);

    std::thread::sleep(Duration::from_millis(150));
    let stdout = collector.stdout_so_far.lock().unwrap().clone();
    assert!(
        stdout.contains("body_bytes=1024"),
        "stdout missing body_bytes=1024: {stdout:?}"
    );
}

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
