//! Placeholder handler for `POST /ingest/v1`. Returns `501 Not
//! Implemented` until the next change wires up body streaming, the
//! schema-version peek, the tmp-file write, the fsync, and the
//! atomic rename. INV-1 (200 only after fsync) means we cannot
//! return 200 from here.
//!
//! When the next change lands it will MODIFY the corresponding spec
//! requirement to demand `200 OK` on success.

use axum::body::Body;
use axum::http::{header, Response, StatusCode};

const NOT_IMPLEMENTED_BODY: &str =
    r#"{"error":"not_yet_implemented","detail":"body handling lands in the next change"}"#;

pub async fn ingest() -> Response<Body> {
    Response::builder()
        .status(StatusCode::NOT_IMPLEMENTED)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(NOT_IMPLEMENTED_BODY))
        .expect("placeholder response is a fixed shape; build cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn placeholder_returns_501_with_documented_body() {
        let resp = ingest().await;
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
        let ct = resp.headers().get(header::CONTENT_TYPE).unwrap();
        assert_eq!(ct, "application/json");

        let body_bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let body = std::str::from_utf8(&body_bytes).unwrap();
        assert!(body.contains("not_yet_implemented"));
        assert!(body.contains("body handling lands in the next change"));
    }
}
