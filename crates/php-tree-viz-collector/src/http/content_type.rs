//! Content-Type-header check: rejects with `415 Unsupported Media
//! Type` unless the request's `Content-Type` media-type token
//! (case-insensitive, parameters tolerated) is
//! `application/vnd.php-analyze.v1+msgpack`.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderMap, Response, StatusCode};
use axum::middleware::Next;

const EXPECTED_MEDIA_TYPE: &str = "application/vnd.php-analyze.v1+msgpack";
const UNSUPPORTED_BODY: &str = r#"{"error":"unsupported_content_type"}"#;

pub async fn require_msgpack_content_type(
    headers: HeaderMap,
    req: Request,
    next: Next,
) -> Response<Body> {
    if !has_expected_content_type(&headers) {
        return unsupported_media_type();
    }
    next.run(req).await
}

fn has_expected_content_type(headers: &HeaderMap) -> bool {
    let Some(value) = headers.get(header::CONTENT_TYPE) else {
        return false;
    };
    let Ok(text) = value.to_str() else {
        return false;
    };
    // Split off any media-type parameters (e.g. `; charset=binary`).
    let media_type = text.split(';').next().unwrap_or("").trim();
    media_type.eq_ignore_ascii_case(EXPECTED_MEDIA_TYPE)
}

fn unsupported_media_type() -> Response<Body> {
    Response::builder()
        .status(StatusCode::UNSUPPORTED_MEDIA_TYPE)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(UNSUPPORTED_BODY))
        .expect("unsupported_media_type response is a fixed shape; build cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with_ct(value: &'static str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static(value));
        h
    }

    #[test]
    fn missing_content_type_is_rejected() {
        assert!(!has_expected_content_type(&HeaderMap::new()));
    }

    #[test]
    fn exact_match_is_accepted() {
        assert!(has_expected_content_type(&headers_with_ct(
            "application/vnd.php-analyze.v1+msgpack"
        )));
    }

    #[test]
    fn uppercase_media_type_is_accepted() {
        assert!(has_expected_content_type(&headers_with_ct(
            "APPLICATION/VND.PHP-ANALYZE.V1+MSGPACK"
        )));
    }

    #[test]
    fn mixed_case_media_type_is_accepted() {
        assert!(has_expected_content_type(&headers_with_ct(
            "Application/VND.php-Analyze.v1+MsgPack"
        )));
    }

    #[test]
    fn trailing_parameters_are_tolerated() {
        assert!(has_expected_content_type(&headers_with_ct(
            "application/vnd.php-analyze.v1+msgpack; charset=binary"
        )));
    }

    #[test]
    fn whitespace_around_media_type_is_trimmed() {
        assert!(has_expected_content_type(&headers_with_ct(
            "  application/vnd.php-analyze.v1+msgpack  "
        )));
    }

    #[test]
    fn wrong_media_type_is_rejected() {
        assert!(!has_expected_content_type(&headers_with_ct(
            "application/json"
        )));
        assert!(!has_expected_content_type(&headers_with_ct("text/plain")));
        assert!(!has_expected_content_type(&headers_with_ct(
            "application/msgpack"
        )));
    }

    #[test]
    fn response_has_correct_status_and_body() {
        let resp = unsupported_media_type();
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let ct = resp.headers().get(header::CONTENT_TYPE).unwrap();
        assert_eq!(ct, "application/json");
    }
}
