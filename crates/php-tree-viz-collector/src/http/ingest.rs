//! Streaming `POST /ingest/v1` handler.
//!
//! Reads the request body frame-by-frame and writes it to
//! `<tmp_dir>/<random>.partial`. Two cap-enforcement paths:
//!
//! - **Fast-path 413**: a `Content-Length` header above the cap
//!   short-circuits before any body bytes are read.
//! - **Running-count 413**: each frame increments a counter; once it
//!   exceeds the cap we delete the partial file, abort the read, and
//!   return `413` with `Connection: close`.
//!
//! On success (body fully read, within cap) the handler still
//! returns `501 Not Implemented` per the interim contract — fsync
//! and atomic rename land in the next change. The partial file is
//! left on disk for that change to pick up.
//!
//! Internal write failures become `500` with a documented JSON body.
//! The path that failed is logged so the operator can find it.
//!
//! On every response the handler attaches a `BodyBytes(u64)`
//! extension carrying the count of bytes actually read from the
//! wire; the request-logging middleware turns that into the
//! `body_bytes=<N>` field on the log line.

use std::path::Path;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, Response, StatusCode};
use http_body_util::BodyExt;
use tokio::io::AsyncWriteExt;

use super::tmp;
use super::{BodyBytes, HttpError, SharedState};

const NOT_IMPLEMENTED_BODY: &str =
    r#"{"error":"not_yet_implemented","detail":"body handling lands in the next change"}"#;
const TOO_LARGE_BODY: &str = r#"{"error":"too_large"}"#;
const INTERNAL_BODY: &str = r#"{"error":"internal","detail":"could not buffer request"}"#;
const MALFORMED_REQUEST_BODY: &str = r#"{"error":"malformed_request"}"#;

pub async fn ingest(
    State(state): State<SharedState>,
    headers: HeaderMap,
    req: Request,
) -> Response<Body> {
    let mut bytes_read: u64 = 0;

    // Fast-path 413: declared Content-Length above the cap means we
    // never open a file or read a byte.
    if let Some(declared) = declared_content_length(&headers) {
        if declared > state.max_body_bytes {
            return with_body_bytes(too_large_response(), 0);
        }
    }

    let filename = tmp::make_filename();
    let path = state.tmp_dir.join(format!("{filename}.partial"));

    let mut file = match open_partial(&path).await {
        Ok(f) => f,
        Err(err) => {
            log_internal_error(&err);
            return with_body_bytes(internal_response(), 0);
        }
    };

    let mut body = req.into_body();
    let outcome = stream_body_to_file(
        &mut body,
        &mut file,
        &path,
        state.max_body_bytes,
        &mut bytes_read,
    )
    .await;

    let response = match outcome {
        Outcome::Ok => {
            if let Err(err) = file.flush().await {
                let _ = tokio::fs::remove_file(&path).await;
                log_internal_error(&HttpError::TmpWrite {
                    path: path.clone(),
                    source: err,
                });
                internal_response()
            } else {
                drop(file);
                not_yet_implemented_response()
            }
        }
        Outcome::OverCap => {
            let _ = tokio::fs::remove_file(&path).await;
            too_large_response()
        }
        Outcome::FrameError => {
            let _ = tokio::fs::remove_file(&path).await;
            malformed_request_response()
        }
        Outcome::WriteError(err) => {
            let _ = tokio::fs::remove_file(&path).await;
            log_internal_error(&err);
            internal_response()
        }
    };

    with_body_bytes(response, bytes_read)
}

#[derive(Debug)]
enum Outcome {
    Ok,
    OverCap,
    FrameError,
    WriteError(HttpError),
}

/// Drive the body stream until it's fully consumed, the cap is
/// exceeded, or an error occurs. Writes each data frame to `file`
/// and increments `*bytes_read` by the frame size as bytes are
/// received from the wire (so the count is accurate even on a 413
/// abort).
async fn stream_body_to_file(
    body: &mut Body,
    file: &mut tokio::fs::File,
    path: &Path,
    cap: u64,
    bytes_read: &mut u64,
) -> Outcome {
    while let Some(frame_result) = body.frame().await {
        let frame = match frame_result {
            Ok(f) => f,
            Err(_) => return Outcome::FrameError,
        };
        let data = match frame.into_data() {
            Ok(d) => d,
            Err(_) => continue, // trailer or other non-data frame
        };
        *bytes_read += data.len() as u64;
        if *bytes_read > cap {
            return Outcome::OverCap;
        }
        if let Err(source) = file.write_all(data.as_ref()).await {
            return Outcome::WriteError(HttpError::TmpWrite {
                path: path.to_path_buf(),
                source,
            });
        }
    }
    Outcome::Ok
}

fn declared_content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

async fn open_partial(path: &Path) -> Result<tokio::fs::File, HttpError> {
    // `tokio::fs::OpenOptions` doesn't expose `.mode()` directly on
    // Unix, so we build a `std::fs::OpenOptions`, apply the mode via
    // the platform-specific `OpenOptionsExt`, open synchronously,
    // and hand the resulting handle to tokio. The blocking `open`
    // is a single syscall and acceptable on the async path.
    let mut opts = std::fs::OpenOptions::new();
    opts.create_new(true).write(true);
    apply_partial_mode(&mut opts);
    let std_file = opts.open(path).map_err(|source| HttpError::TmpWrite {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(tokio::fs::File::from_std(std_file))
}

#[cfg(unix)]
fn apply_partial_mode(opts: &mut std::fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    opts.mode(0o600);
}

#[cfg(not(unix))]
fn apply_partial_mode(_opts: &mut std::fs::OpenOptions) {}

fn with_body_bytes(mut resp: Response<Body>, bytes: u64) -> Response<Body> {
    resp.extensions_mut().insert(BodyBytes(bytes));
    resp
}

fn not_yet_implemented_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::NOT_IMPLEMENTED)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(NOT_IMPLEMENTED_BODY))
        .expect("placeholder response is a fixed shape; build cannot fail")
}

fn too_large_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::PAYLOAD_TOO_LARGE)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONNECTION, "close")
        .body(Body::from(TOO_LARGE_BODY))
        .expect("413 response is a fixed shape; build cannot fail")
}

fn internal_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONNECTION, "close")
        .body(Body::from(INTERNAL_BODY))
        .expect("500 response is a fixed shape; build cannot fail")
}

fn malformed_request_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONNECTION, "close")
        .body(Body::from(MALFORMED_REQUEST_BODY))
        .expect("400 response is a fixed shape; build cannot fail")
}

fn log_internal_error(err: &HttpError) {
    // Mirror the single-line stderr convention used elsewhere in the
    // crate. The `obs` sub-module will swap this for structured
    // logging when it lands.
    eprintln!("http error during ingest: {err}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with_cl(value: &'static str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::CONTENT_LENGTH, HeaderValue::from_static(value));
        h
    }

    #[test]
    fn declared_content_length_parses_valid_numbers() {
        assert_eq!(declared_content_length(&headers_with_cl("0")), Some(0));
        assert_eq!(
            declared_content_length(&headers_with_cl("1024")),
            Some(1024)
        );
        assert_eq!(
            declared_content_length(&headers_with_cl("18446744073709551615")), // u64::MAX
            Some(u64::MAX)
        );
    }

    #[test]
    fn declared_content_length_rejects_invalid_values() {
        assert_eq!(declared_content_length(&HeaderMap::new()), None);
        assert_eq!(declared_content_length(&headers_with_cl("xyz")), None);
        assert_eq!(declared_content_length(&headers_with_cl("-1")), None);
    }

    #[test]
    fn too_large_response_carries_connection_close() {
        let r = too_large_response();
        assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let conn = r.headers().get(header::CONNECTION).unwrap();
        assert_eq!(conn, "close");
    }

    #[test]
    fn internal_response_carries_connection_close() {
        let r = internal_response();
        assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let conn = r.headers().get(header::CONNECTION).unwrap();
        assert_eq!(conn, "close");
    }

    #[tokio::test]
    async fn placeholder_returns_501_with_documented_body() {
        let r = not_yet_implemented_response();
        assert_eq!(r.status(), StatusCode::NOT_IMPLEMENTED);

        let body_bytes = axum::body::to_bytes(r.into_body(), 1024).await.unwrap();
        let body = std::str::from_utf8(&body_bytes).unwrap();
        assert!(body.contains("not_yet_implemented"));
        assert!(body.contains("body handling lands in the next change"));
    }

    #[test]
    fn body_bytes_extension_round_trips() {
        let r = with_body_bytes(not_yet_implemented_response(), 42);
        let extension = r.extensions().get::<BodyBytes>().copied();
        assert_eq!(extension.map(|b| b.0), Some(42));
    }
}
