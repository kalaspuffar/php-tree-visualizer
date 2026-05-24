//! Subprocess harness for integration tests.
//!
//! Mirrors the harness in `tests/http_skeleton.rs` — kept here so the
//! new disk-usage and observability test binaries can drive a real
//! collector subprocess against the same `subscriber` install path
//! the operator runs in production. Subscriber output is captured
//! via the subprocess's stdout, which is the same surface journald
//! reads.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const BIN: &str = env!("CARGO_BIN_EXE_php-tree-viz-collector");
pub const TOKEN: &str = "PHPTVTESTTOKEN1234567890ABCDEFGH1234567890";
pub const SALT: &str = "PHPTVTESTSALT0987654321ZYXWVUTSR0987654321";
pub const MEDIA_TYPE: &str = "application/vnd.php-analyze.v1+msgpack";

/// Event-message substrings. Stay in lockstep with the spec deltas
/// in `openspec/changes/observability-polish/specs/`.
pub const EVENT_BATCH_ACCEPTED: &str = "batch accepted";
pub const EVENT_TRACE_FINALIZED: &str = "trace finalized";
pub const EVENT_RETENTION_SWEPT: &str = "retention swept";
pub const EVENT_CONFIG_LOADED: &str = "configuration loaded";
pub const EVENT_DISK_USAGE: &str = "disk usage";

pub fn unique_tempdir(label: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("phptv-{}-{}-{}", std::process::id(), label, n));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Configuration template + extras. Test binaries call this to write
/// a `collector.toml` and get back the path to pass via `--config`.
pub struct ConfigBuilder {
    pub dir: PathBuf,
    pub bind: String,
    pub retention_days: u32,
    pub fast_finalize: bool,
    pub fast_retention: bool,
    pub disk_capacity_bytes: Option<u64>,
    pub disk_usage_tick_seconds_test_override: Option<u64>,
    pub disk_usage_warn_pct: Option<u8>,
}

impl ConfigBuilder {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            bind: "127.0.0.1:0".to_owned(),
            retention_days: 30,
            fast_finalize: false,
            fast_retention: false,
            disk_capacity_bytes: None,
            disk_usage_tick_seconds_test_override: None,
            disk_usage_warn_pct: None,
        }
    }

    pub fn fast_finalize(mut self) -> Self {
        self.fast_finalize = true;
        self
    }

    pub fn fast_retention(mut self, retention_days: u32) -> Self {
        self.fast_retention = true;
        self.retention_days = retention_days;
        // Fast-retention also implies fast-finalize so a slow tick
        // doesn't race the retention assertion.
        self.fast_finalize = true;
        self
    }

    pub fn disk_capacity_bytes(mut self, n: u64) -> Self {
        self.disk_capacity_bytes = Some(n);
        self
    }

    pub fn disk_usage_test_override(mut self, n: u64) -> Self {
        self.disk_usage_tick_seconds_test_override = Some(n);
        self
    }

    pub fn disk_usage_warn_pct(mut self, pct: u8) -> Self {
        self.disk_usage_warn_pct = Some(pct);
        self
    }

    pub fn write(self) -> PathBuf {
        let data_dir = self.dir.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let bind = &self.bind;
        let storage_extra = match self.disk_capacity_bytes {
            Some(n) => format!("disk_capacity_bytes = {n}\n"),
            None => String::new(),
        };
        let finalize_section = if self.fast_finalize {
            "[finalize]\nidle_seconds = 1\ntick_seconds = 1\n"
        } else {
            ""
        };
        let retention_section = if self.fast_retention {
            "[retention]\ntick_minutes = 60\ntick_seconds = 1\n"
        } else {
            ""
        };
        let mut observability_section = String::new();
        if self.disk_usage_tick_seconds_test_override.is_some()
            || self.disk_usage_warn_pct.is_some()
        {
            observability_section.push_str("[observability]\n");
            if let Some(n) = self.disk_usage_tick_seconds_test_override {
                observability_section
                    .push_str(&format!("disk_usage_tick_seconds_test_override = {n}\n"));
            }
            if let Some(pct) = self.disk_usage_warn_pct {
                observability_section.push_str(&format!("disk_usage_warn_pct = {pct}\n"));
            }
        }
        let retention_days = self.retention_days;
        let body = format!(
            r#"[server]
bind = "{bind}"

[auth]
token = "{TOKEN}"
session_salt = "{SALT}"

[storage]
data_dir = "{data_dir}"
retention_days = {retention_days}
{storage_extra}
{finalize_section}{retention_section}
[log]
format = "text"

{observability_section}"#,
            data_dir = data_dir.display(),
        );
        let path = self.dir.join("collector.toml");
        std::fs::write(&path, body).unwrap();
        path
    }
}

/// RAII handle that owns a spawned collector. Sends SIGTERM and then
/// waits on `Drop`, falling back to SIGKILL if the process is
/// stubborn. Captures stdout/stderr for inspection.
pub struct Collector {
    child: Option<Child>,
    pub bound: String,
    pub stdout_so_far: Arc<Mutex<String>>,
    pub stderr_so_far: Arc<Mutex<String>>,
}

impl Collector {
    pub fn spawn(config_path: &Path) -> Self {
        Self::spawn_with_env(config_path, &[])
    }

    /// Same as `spawn`, but lets a test set extra environment variables
    /// on the subprocess. Used by the sd_notify integration test to
    /// inject `NOTIFY_SOCKET`.
    pub fn spawn_with_env(config_path: &Path, env: &[(&str, &str)]) -> Self {
        let mut cmd = Command::new(BIN);
        cmd.arg("--config")
            .arg(config_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("failed to launch the collector binary");

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let stdout_buf = Arc::new(Mutex::new(String::new()));
        let stderr_buf = Arc::new(Mutex::new(String::new()));

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

        // Drain stdout in a background thread; poll the captured
        // buffer with a deadline. See http_skeleton's identical
        // pattern for why we don't block on `read_line` in the
        // wait loop.
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
            let snapshot = stdout_buf.lock().unwrap().clone();
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

    pub fn wait_for_stdout(&self, substring: &str, timeout: Duration) -> String {
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
            let _ = Command::new("kill")
                .args(["-TERM", &child.id().to_string()])
                .status();
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

pub fn send_raw(host: &str, request: &[u8]) -> (u16, String) {
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
    let code = status_line
        .split_whitespace()
        .nth(1)
        .expect("status line missing code")
        .parse()
        .expect("status code not an integer");
    (code, body)
}

pub fn request(
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    host: &str,
) -> Vec<u8> {
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
    let mut out = req.into_bytes();
    out.extend_from_slice(body);
    out
}

pub fn ingest_request(host: &str, body: &[u8]) -> Vec<u8> {
    let bearer = format!("Bearer {TOKEN}");
    request(
        "POST",
        "/ingest/v1",
        &[("Authorization", &bearer), ("Content-Type", MEDIA_TYPE)],
        body,
        host,
    )
}

pub fn extract_field(line: &str, field: &str) -> Option<String> {
    let needle = format!(" {field}=");
    let pos = line.find(&needle)?;
    let value_start = pos + needle.len();
    let tail = &line[value_start..];
    let value_end = tail.find(|c: char| c.is_whitespace()).unwrap_or(tail.len());
    Some(tail[..value_end].to_owned())
}
