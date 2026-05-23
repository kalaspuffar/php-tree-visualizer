//! Binds the listener, prints the startup banner, runs axum, and
//! shuts down gracefully on SIGTERM or SIGINT.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpListener;

use super::{router, tmp, AppState, HttpError, SharedState};
use crate::config::Config;

/// Entry point called from `main` after the config has been validated.
/// Returns only when the server stops (graceful shutdown → `Ok`,
/// any other failure → `Err`).
pub async fn run(config: Arc<Config>) -> Result<(), HttpError> {
    // `Config::validate` already asserted this parses and is loopback.
    let addr: SocketAddr = config
        .server
        .bind
        .parse()
        .expect("bind validated by Config::validate; parse cannot fail here");

    // Prepare the tmp directory *before* binding so any I/O failure
    // here exits with status 3 before any client could connect.
    let tmp_dir = tmp::ensure_clean_tmp_dir(&config.storage.data_dir)?;

    let listener = TcpListener::bind(addr)
        .await
        .map_err(|source| HttpError::Bind { addr, source })?;
    let bound = listener
        .local_addr()
        .map_err(|source| HttpError::Bind { addr, source })?;

    // Install signal handlers *before* we announce readiness on
    // stdout. Otherwise a SIGTERM arriving in the gap between
    // "listening on …" and axum's first poll of the shutdown future
    // would hit the kernel default action — terminate — and the
    // process would exit with a signal status rather than `0`.
    let shutdown = build_shutdown_signal()?;

    println!("listening on {bound}");

    let state: SharedState = Arc::new(AppState {
        expected_token: config.auth.token.clone(),
        max_body_bytes: config.server.max_body_bytes,
        tmp_dir,
    });

    let app = router::build(state).into_make_service_with_connect_info::<SocketAddr>();

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(HttpError::Serve)?;

    println!("shutdown complete");
    Ok(())
}

/// Install the SIGINT and SIGTERM handlers eagerly and return a
/// future that resolves on whichever fires first. The handlers are
/// installed by `signal::unix::signal(...)`'s side effect — once that
/// call returns, the default action (terminate) is replaced by
/// delivery to the tokio runtime.
#[cfg(unix)]
fn build_shutdown_signal() -> Result<impl Future<Output = ()>, HttpError> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigint = signal(SignalKind::interrupt()).map_err(HttpError::Serve)?;
    let mut sigterm = signal(SignalKind::terminate()).map_err(HttpError::Serve)?;
    Ok(async move {
        tokio::select! {
            _ = sigint.recv() => println!("shutdown: SIGINT received"),
            _ = sigterm.recv() => println!("shutdown: SIGTERM received"),
        }
    })
}

/// Non-Unix fallback — only SIGINT (Ctrl+C) is observed.
#[cfg(not(unix))]
fn build_shutdown_signal() -> Result<impl Future<Output = ()>, HttpError> {
    Ok(async move {
        if let Err(e) = tokio::signal::ctrl_c().await {
            eprintln!("shutdown: ctrl_c listener failed: {e}");
        }
        println!("shutdown: SIGINT received");
    })
}
