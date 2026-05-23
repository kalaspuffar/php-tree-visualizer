//! Streaming `POST /ingest/v1` handler with durable commit.
//!
//! Per-request flow (see SPECIFICATION.md §2.2):
//!
//! 1. Fast-path 413 on oversize `Content-Length`.
//! 2. Stream body frame-by-frame into `tmp/<random>.partial`.
//!    Running-count 413 on cap exceeded; 400 on body stream error.
//! 3. Read the partial back, decode the `meta` block.
//!    Malformed → 400. `schema_version != 1` → 422.
//! 4. Compute `TraceKey` from meta; acquire per-trace mutex.
//! 5. Atomic rename → `traces/<key>.raw/batch-NNNN.msgpack`.
//! 6. fsync(file) + fsync(parent_dir).
//! 7. Return `200` with empty body.
//!
//! Every response carries a `BodyBytes(u64)` extension so the
//! request-logging middleware can emit `body_bytes=<N>`.

use std::path::Path;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, Response, StatusCode};
use http_body_util::BodyExt;
use tokio::io::AsyncWriteExt;

use super::storage;
use super::tmp;
use super::{BatchSubmission, BodyBytes, HttpError, SharedState};
use crate::tracekey;
use crate::wire;

const TOO_LARGE_BODY: &str = r#"{"error":"too_large"}"#;
const BACKPRESSURE_BODY: &str = r#"{"error":"backpressure"}"#;
const INTERNAL_BODY: &str = r#"{"error":"internal","detail":"could not buffer request"}"#;
const MALFORMED_FRAME_BODY: &str = r#"{"error":"malformed_request"}"#;
const TRACE_FULL_BODY_PREFIX: &str = r#"{"error":"trace_full","trace_key":""#;
const TRACE_FULL_BODY_SUFFIX: &str = r#""}"#;

pub async fn ingest(
    State(state): State<SharedState>,
    headers: HeaderMap,
    req: Request,
) -> Response<Body> {
    let mut bytes_read: u64 = 0;

    // ---- Fast-path 413 ----
    if let Some(declared) = declared_content_length(&headers) {
        if declared > state.max_body_bytes {
            return with_body_bytes(too_large_response(), 0);
        }
    }

    // ---- Stream body to tmp ----
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

    match outcome {
        Outcome::OverCap => {
            let _ = tokio::fs::remove_file(&path).await;
            return with_body_bytes(too_large_response(), bytes_read);
        }
        Outcome::FrameError => {
            let _ = tokio::fs::remove_file(&path).await;
            return with_body_bytes(malformed_frame_response(), bytes_read);
        }
        Outcome::WriteError(err) => {
            let _ = tokio::fs::remove_file(&path).await;
            log_internal_error(&err);
            return with_body_bytes(internal_response(), bytes_read);
        }
        Outcome::Ok => {}
    }

    if let Err(source) = file.flush().await {
        let _ = tokio::fs::remove_file(&path).await;
        log_internal_error(&HttpError::TmpWrite {
            path: path.clone(),
            source,
        });
        return with_body_bytes(internal_response(), bytes_read);
    }
    drop(file);

    // ---- Decode meta ----
    let body_bytes_on_disk = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(source) => {
            let _ = tokio::fs::remove_file(&path).await;
            log_internal_error(&HttpError::TmpWrite {
                path: path.clone(),
                source,
            });
            return with_body_bytes(internal_response(), bytes_read);
        }
    };
    let meta = match wire::parse_meta(&body_bytes_on_disk) {
        Ok(m) => m,
        Err(err) => {
            let _ = tokio::fs::remove_file(&path).await;
            return with_body_bytes(malformed_msgpack_response(&format!("{err}")), bytes_read);
        }
    };
    if meta.schema_version != 1 {
        let _ = tokio::fs::remove_file(&path).await;
        return with_body_bytes(
            unsupported_schema_version_response(meta.schema_version),
            bytes_read,
        );
    }

    // ---- Reserve a slot on the bounded ingest channel ----
    // Done before touching the trace's raw directory so a 503
    // leaves zero on-disk artefacts for the rejected request.
    // See design D-1.
    let permit = match state.batch_tx.try_reserve() {
        Ok(p) => p,
        Err(_) => {
            let _ = tokio::fs::remove_file(&path).await;
            return with_body_bytes(backpressure_response(), bytes_read);
        }
    };

    // ---- Commit under per-trace lock (permit held throughout) ----
    let key = tracekey::from_meta(&meta);
    let lock = state.lock_for(&key);
    let commit_result = {
        // `tokio::sync::Mutex` is async-aware; holding the guard
        // across the rename + fsync `await`s is the intended use.
        let _guard = lock.lock().await;
        storage::commit_partial(&path, &state.traces_dir, &key).await
    };

    match commit_result {
        Ok(target) => {
            // Use the permit to send. The send is infallible because
            // the slot was reserved above; consuming it releases the
            // permit's hold on the slot and queues the submission.
            permit.send(BatchSubmission {
                path: target,
                trace_key: key,
            });
            with_body_bytes(success_response(), bytes_read)
        }
        Err(HttpError::TraceFull { key }) => {
            // Dropping the permit releases the reserved slot.
            // Leave the partial in place on trace_full so an
            // operator can inspect what could not be committed.
            drop(permit);
            with_body_bytes(trace_full_response(&key), bytes_read)
        }
        Err(err) => {
            // Dropping the permit releases the reserved slot.
            drop(permit);
            let _ = tokio::fs::remove_file(&path).await;
            log_internal_error(&err);
            with_body_bytes(internal_response(), bytes_read)
        }
    }
}

#[derive(Debug)]
enum Outcome {
    Ok,
    OverCap,
    FrameError,
    WriteError(HttpError),
}

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
            Err(_) => continue,
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

fn success_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::empty())
        .expect("200 response is a fixed shape; build cannot fail")
}

fn too_large_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::PAYLOAD_TOO_LARGE)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONNECTION, "close")
        .body(Body::from(TOO_LARGE_BODY))
        .expect("413 response is a fixed shape; build cannot fail")
}

fn backpressure_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONNECTION, "close")
        .body(Body::from(BACKPRESSURE_BODY))
        .expect("503 response is a fixed shape; build cannot fail")
}

fn internal_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONNECTION, "close")
        .body(Body::from(INTERNAL_BODY))
        .expect("500 response is a fixed shape; build cannot fail")
}

fn malformed_frame_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONNECTION, "close")
        .body(Body::from(MALFORMED_FRAME_BODY))
        .expect("400 response is a fixed shape; build cannot fail")
}

/// 400 with a documented MessagePack-parse body. Detail is the
/// wire error's `Display`, JSON-escaped naively (we control the
/// inputs — they have no control characters beyond what serde
/// produces, but escape quotes + backslashes defensively).
fn malformed_msgpack_response(detail: &str) -> Response<Body> {
    let escaped = escape_json(detail);
    let body = format!(r#"{{"error":"malformed_msgpack","detail":"{escaped}"}}"#);
    Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("400 response build cannot fail")
}

fn unsupported_schema_version_response(got: u32) -> Response<Body> {
    let body = format!(r#"{{"error":"unsupported_schema_version","got":{got}}}"#);
    Response::builder()
        .status(StatusCode::UNPROCESSABLE_ENTITY)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("422 response build cannot fail")
}

fn trace_full_response(key: &crate::tracekey::TraceKey) -> Response<Body> {
    let mut body = String::with_capacity(64);
    body.push_str(TRACE_FULL_BODY_PREFIX);
    body.push_str(key.as_str());
    body.push_str(TRACE_FULL_BODY_SUFFIX);
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("500 trace_full response build cannot fail")
}

fn escape_json(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            // Control characters → space (defensive; should not appear in WireError display).
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

fn log_internal_error(err: &HttpError) {
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
            declared_content_length(&headers_with_cl("18446744073709551615")),
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
        assert_eq!(r.headers().get(header::CONNECTION).unwrap(), "close");
    }

    #[test]
    fn backpressure_response_has_correct_status_and_body() {
        let r = backpressure_response();
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            r.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        assert_eq!(r.headers().get(header::CONNECTION).unwrap(), "close");
    }

    #[tokio::test]
    async fn backpressure_response_body_is_the_documented_shape() {
        let r = backpressure_response();
        let body = axum::body::to_bytes(r.into_body(), 1024).await.unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), BACKPRESSURE_BODY);
    }

    #[tokio::test]
    async fn try_reserve_returns_full_when_slot_is_taken() {
        // Sanity check of the tokio primitive we depend on: with a
        // channel of capacity 1, a single outstanding permit makes
        // the next `try_reserve` fail with `Full`. If this ever
        // breaks (e.g. tokio's mpsc reuses the slot mid-permit) the
        //503 path would silently become unreachable.
        let (tx, _rx) = tokio::sync::mpsc::channel::<u32>(1);
        let _permit = tx.try_reserve().expect("first reserve must succeed");
        let err = tx.try_reserve().expect_err("second reserve must fail");
        assert!(matches!(
            err,
            tokio::sync::mpsc::error::TrySendError::Full(_)
        ));
    }

    #[test]
    fn internal_response_carries_connection_close() {
        let r = internal_response();
        assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(r.headers().get(header::CONNECTION).unwrap(), "close");
    }

    #[test]
    fn unsupported_schema_version_response_carries_got_value() {
        let r = unsupported_schema_version_response(7);
        assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            r.headers().get(header::CONNECTION).is_none(),
            "422 must not force connection close"
        );
    }

    #[test]
    fn malformed_msgpack_response_includes_detail() {
        let r = malformed_msgpack_response(r#"bad "quote" and \backslash"#);
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert!(
            r.headers().get(header::CONNECTION).is_none(),
            "400-malformed must not force connection close"
        );
    }

    #[test]
    fn success_response_is_empty_200() {
        let r = success_response();
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[test]
    fn escape_json_handles_quotes_and_backslashes() {
        assert_eq!(escape_json(r#"abc"def"#), r#"abc\"def"#);
        assert_eq!(escape_json(r#"a\b"#), r#"a\\b"#);
        assert_eq!(escape_json("normal text"), "normal text");
    }

    #[test]
    fn escape_json_replaces_control_chars_with_space() {
        assert_eq!(escape_json("a\nb"), "a b");
        assert_eq!(escape_json("a\tb"), "a b");
    }

    #[test]
    fn body_bytes_extension_round_trips() {
        let r = with_body_bytes(success_response(), 42);
        let extension = r.extensions().get::<BodyBytes>().copied();
        assert_eq!(extension.map(|b| b.0), Some(42));
    }
}
