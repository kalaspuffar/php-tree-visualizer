//! Error type produced by the HTTP layer (bind, serve, shutdown).
//!
//! Same single-line `Display` contract and `Error::source` chain that
//! `config::ConfigError` follows, so the operator sees one
//! human-readable line on stderr regardless of which subsystem
//! refused to start.

use std::net::SocketAddr;

#[derive(Debug)]
pub enum HttpError {
    /// `TcpListener::bind` failed (address in use, permission denied,
    /// kernel ran out of file descriptors, etc.).
    Bind {
        addr: SocketAddr,
        source: std::io::Error,
    },
    /// The HTTP server task itself returned an I/O error during
    /// `axum::serve(...).await`.
    Serve(std::io::Error),
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bind { addr, source } => {
                write!(f, "could not bind {addr}: {source}")
            }
            Self::Serve(source) => write!(f, "http server failed: {source}"),
        }
    }
}

impl std::error::Error for HttpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Bind { source, .. } | Self::Serve(source) => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_bind_contains_addr_and_reason() {
        let addr: SocketAddr = "127.0.0.1:8088".parse().unwrap();
        let err = HttpError::Bind {
            addr,
            source: std::io::Error::new(std::io::ErrorKind::AddrInUse, "address in use"),
        };
        let rendered = format!("{err}");
        assert!(rendered.contains("127.0.0.1:8088"));
        assert!(rendered.contains("address in use"));
        assert!(!rendered.contains('\n'));
    }

    #[test]
    fn source_is_set_for_every_variant() {
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let bind = HttpError::Bind {
            addr,
            source: std::io::Error::new(std::io::ErrorKind::AddrInUse, "x"),
        };
        let serve = HttpError::Serve(std::io::Error::other("x"));
        assert!(std::error::Error::source(&bind).is_some());
        assert!(std::error::Error::source(&serve).is_some());
    }
}
