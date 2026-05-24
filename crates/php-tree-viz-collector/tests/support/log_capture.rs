//! In-process capture for `tracing-subscriber` output.
//!
//! Used by tests that run the collector's loops in-process (so they
//! can install their own subscriber) and want to assert on the
//! structured event stream. For subprocess-based tests, parse the
//! child's stdout/stderr directly — this helper does not work
//! across process boundaries.

use std::io;
use std::sync::{Arc, Mutex, MutexGuard};

use tracing_subscriber::fmt::MakeWriter;

/// A thread-safe in-memory log sink. Implements
/// `tracing_subscriber::fmt::MakeWriter` so it can be plugged into
/// `tracing_subscriber::fmt().with_writer(buffer.make_writer())`.
#[derive(Clone, Default)]
pub struct LogBuffer {
    inner: Arc<Mutex<Vec<u8>>>,
}

impl LogBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot the captured bytes as a `String`. Used by assertions.
    pub fn as_string(&self) -> String {
        let bytes = self.inner.lock().expect("log buffer not poisoned").clone();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Snapshot the captured bytes. Used by tests that want to assert
    /// against the raw byte stream (e.g. the token-leak guard).
    pub fn as_bytes(&self) -> Vec<u8> {
        self.inner.lock().expect("log buffer not poisoned").clone()
    }
}

impl<'a> MakeWriter<'a> for LogBuffer {
    type Writer = LogBufferWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        LogBufferWriter {
            buffer: self.inner.lock().expect("log buffer not poisoned"),
        }
    }
}

pub struct LogBufferWriter<'a> {
    buffer: MutexGuard<'a, Vec<u8>>,
}

impl io::Write for LogBufferWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
