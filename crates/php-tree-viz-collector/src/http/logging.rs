//! Per-request log middleware.
//!
//! Emits exactly one line per request to stdout *after* the response
//! is generated, so the line carries the final status code (including
//! the 401 / 415 rejections produced by upstream middleware) and the
//! body-byte count attached by the ingest handler. The line contains:
//! a Unix timestamp, method, path, remote address, status, and
//! `body_bytes=<N>`. Header content and request/response bodies are
//! never included — *INV-2*, `SPECIFICATION.md` §6.4.
//!
//! Timestamp format is the seconds-since-epoch integer for now; the
//! `obs` sub-module of §3.1 will swap this for structured RFC3339
//! output when it lands.

use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::http::Response;
use axum::middleware::Next;

use super::BodyBytes;

pub async fn log_request(
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response<Body> {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let response = next.run(req).await;
    let status = response.status().as_u16();
    let body_bytes = response
        .extensions()
        .get::<BodyBytes>()
        .map(|b| b.0)
        .unwrap_or(0);
    println!(
        "{}",
        format_log_line(
            unix_secs(),
            method.as_ref(),
            &path,
            &remote.to_string(),
            status,
            body_bytes,
        ),
    );
    response
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Pure formatter so unit tests can exercise it without an HTTP stack.
pub(crate) fn format_log_line(
    ts: u64,
    method: &str,
    path: &str,
    remote: &str,
    status: u16,
    body_bytes: u64,
) -> String {
    format!(
        "time={ts} method={method} path={path} remote={remote} status={status} body_bytes={body_bytes}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_has_expected_shape_and_contains_no_header_content() {
        let line = format_log_line(1700000000, "POST", "/ingest/v1", "127.0.0.1:54321", 401, 0);
        assert_eq!(
            line,
            "time=1700000000 method=POST path=/ingest/v1 remote=127.0.0.1:54321 status=401 body_bytes=0"
        );
        // Defensive — the formatter has no access to header values, so
        // it cannot leak them. Belt-and-braces assertion.
        assert!(!line.to_lowercase().contains("authorization"));
        assert!(!line.to_lowercase().contains("bearer"));
    }

    #[test]
    fn line_includes_body_bytes_field() {
        let line = format_log_line(1, "POST", "/ingest/v1", "1.2.3.4:5", 501, 1024);
        assert!(line.contains("body_bytes=1024"), "{line}");
    }

    #[test]
    fn line_is_a_single_line() {
        let line = format_log_line(1, "POST", "/p", "1.2.3.4:5", 200, 0);
        assert!(!line.contains('\n'));
        assert!(!line.contains('\r'));
    }
}
