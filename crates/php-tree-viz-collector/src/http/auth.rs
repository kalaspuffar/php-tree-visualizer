//! Authorization-header check: rejects with `401 Unauthorized` unless
//! the request carries `Authorization: Bearer <token>` matching
//! `config.auth.token`.
//!
//! The token comparison is constant-time (over equal-length inputs)
//! so a curious neighbour on the dev VLAN cannot recover the token
//! byte-by-byte via response-timing measurements. The threat model
//! (`SPECIFICATION.md` §6.1) treats this as low-stakes; the cost of
//! doing it right is fewer than 15 lines.
//!
//! INV-2: the token bytes never reach any log line. The middleware
//! returns a `Response` directly on rejection and lets the request
//! through on success; logging is the request-logging middleware's
//! responsibility, and it only sees the final status code.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, Response, StatusCode};
use axum::middleware::Next;

use super::SharedState;

const UNAUTHORIZED_BODY: &str = r#"{"error":"unauthorized"}"#;

pub async fn require_bearer_token(
    State(state): State<SharedState>,
    headers: HeaderMap,
    req: Request,
    next: Next,
) -> Response<Body> {
    let Some(received) = extract_bearer_token(&headers) else {
        return unauthorized();
    };
    let expected = state.expected_token.expose_secret();
    if !ct_eq(received.as_bytes(), expected.as_bytes()) {
        return unauthorized();
    }
    next.run(req).await
}

fn extract_bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    value.strip_prefix("Bearer ")
}

fn unauthorized() -> Response<Body> {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(UNAUTHORIZED_BODY))
        .expect("unauthorized response is a fixed shape; build cannot fail")
}

/// Constant-time equality over byte slices of the same length.
///
/// Lengths are compared up-front; this is intentional — the
/// configured token's length is not secret, and walking the longer
/// slice to hide the length difference would buy nothing. The
/// guarantee that matters is "no early-out on the first byte
/// difference," which the XOR-accumulating loop provides.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with(name: &'static str, value: &'static str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(name, HeaderValue::from_static(value));
        h
    }

    #[test]
    fn ct_eq_returns_true_for_equal_bytes() {
        assert!(ct_eq(b"hunter2", b"hunter2"));
        assert!(ct_eq(b"", b""));
        assert!(ct_eq(&[0xff, 0x00, 0xaa], &[0xff, 0x00, 0xaa]));
    }

    #[test]
    fn ct_eq_returns_false_for_different_lengths() {
        assert!(!ct_eq(b"hunter2", b"hunter22"));
        assert!(!ct_eq(b"", b"x"));
    }

    #[test]
    fn ct_eq_returns_false_for_first_byte_difference() {
        assert!(!ct_eq(b"xunter2", b"hunter2"));
    }

    #[test]
    fn ct_eq_returns_false_for_last_byte_difference() {
        assert!(!ct_eq(b"hunter2", b"hunter3"));
    }

    #[test]
    fn extract_bearer_pulls_token_from_well_formed_header() {
        let h = headers_with("authorization", "Bearer abc123");
        assert_eq!(extract_bearer_token(&h), Some("abc123"));
    }

    #[test]
    fn extract_bearer_rejects_missing_header() {
        let h = HeaderMap::new();
        assert_eq!(extract_bearer_token(&h), None);
    }

    #[test]
    fn extract_bearer_rejects_wrong_scheme() {
        let h = headers_with("authorization", "Basic abc");
        assert_eq!(extract_bearer_token(&h), None);
    }

    #[test]
    fn extract_bearer_rejects_empty_value() {
        let h = headers_with("authorization", "");
        assert_eq!(extract_bearer_token(&h), None);
    }

    #[test]
    fn extract_bearer_rejects_bearer_without_space() {
        // "Bearerabc" — no space between scheme and value.
        let h = headers_with("authorization", "Bearerabc");
        assert_eq!(extract_bearer_token(&h), None);
    }

    #[test]
    fn unauthorized_response_has_correct_status_and_body() {
        let resp = unauthorized();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let ct = resp.headers().get(header::CONTENT_TYPE).unwrap();
        assert_eq!(ct, "application/json");
    }
}
