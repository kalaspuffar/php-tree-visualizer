//! Per-request log middleware.
//!
//! Emits exactly one tracing event per request *after* the response
//! is generated, so the event carries the final status code (including
//! the 401 / 415 rejections produced by upstream middleware) and the
//! body-byte count attached by the ingest handler. The event's fields
//! are: method, path, remote address, status, and `body_bytes`.
//! Header content and request/response bodies are never read by this
//! middleware — *INV-2*, `SPECIFICATION.md` §6.4.

use std::net::SocketAddr;

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
    // INV-2: the middleware reads only the request's method/path,
    // the remote socket, the response status, and the body byte
    // count tagged onto the response by the ingest handler. It
    // never touches the `Authorization` header or any other
    // header content.
    tracing::info!(
        method = %method,
        path = %path,
        remote_addr = %remote,
        status,
        body_bytes,
        "request"
    );
    response
}

#[cfg(test)]
mod tests {
    //! The logging middleware is covered end-to-end by the
    //! integration tests in `tests/http_skeleton.rs` (which inspect
    //! the subscriber output for the structured fields). No unit
    //! tests live here — the formatter is now `tracing-subscriber`'s
    //! responsibility, not ours.
}
