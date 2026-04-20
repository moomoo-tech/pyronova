//! Streaming request body — lets a handler consume `Incoming` body frames
//! as they arrive, without buffering the whole body in memory.
//!
//! Opt-in per route via `@app.post("/path", gil=True, stream=True)`. The
//! accept loop skips `Limited::new().collect()` and instead spawns a feeder
//! task that pushes each body frame into an `std::sync::mpsc` channel; the
//! handler sees `req.stream` as a Python iterator yielding `bytes` chunks
//! and terminating with `StopIteration` on EOF.
//!
//! Scope (v1):
//!   * Only `gil=True` routes. Sub-interpreter streaming needs a C-FFI
//!     bridge akin to `pyre_recv`/`pyre_send` and is deferred.
//!   * Sync iterator only. `async for chunk in req.stream()` is deferred.
//!   * `max_body_size` still bounds total ingest even when streaming.
//!
//! Error handling: if the client disconnects or sends malformed frames,
//! the feeder sends an error message on the channel; `__next__` raises
//! `IOError(msg)` so the handler can terminate cleanly.

use std::sync::Mutex;

use bytes::Bytes;
use pyo3::exceptions::{PyIOError, PyStopIteration};
use pyo3::prelude::*;
use pyo3::types::PyBytes;

/// A message on the feeder → handler channel.
pub(crate) enum ChunkMsg {
    Data(Bytes),
    /// Feeder hit an error (body too large, client disconnected, etc.).
    /// The handler will raise IOError on next iteration.
    Err(String),
    /// End of body — channel sender is dropped after sending this, so
    /// subsequent recv() returns Err immediately.
    Eof,
}

/// Python-visible iterator over an incoming body's chunks.
///
/// The receiver is wrapped in `Mutex<Option<...>>` so that:
///   * iteration is thread-safe (Python's GIL doesn't cover Rust channels)
///   * we can drop the receiver on EOF/error to free resources promptly
///
/// Only one logical iterator should consume a given stream — attempting to
/// call `next()` on a stream that's already been drained yields
/// `StopIteration` immediately.
#[pyclass]
pub(crate) struct PyreBodyStream {
    rx: Mutex<Option<std::sync::mpsc::Receiver<ChunkMsg>>>,
}

impl PyreBodyStream {
    pub(crate) fn new(rx: std::sync::mpsc::Receiver<ChunkMsg>) -> Self {
        PyreBodyStream {
            rx: Mutex::new(Some(rx)),
        }
    }
}

#[pymethods]
impl PyreBodyStream {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    /// Block until the next chunk arrives. Returns `bytes` for data,
    /// raises `StopIteration` at EOF, `IOError(msg)` on transport error.
    fn __next__(&self, py: Python<'_>) -> PyResult<Py<PyBytes>> {
        // Take the receiver out of the Mutex for the blocking recv() call.
        // We can't hold the MutexGuard across a blocking recv() because that
        // would block *other* Python threads trying to interact with the
        // same stream object. Release the GIL during the recv so other
        // threads can make progress.
        let rx_opt = { self.rx.lock().unwrap().take() };
        let Some(rx) = rx_opt else {
            return Err(PyStopIteration::new_err("stream exhausted"));
        };

        // Move rx into the closure (Sync-by-reference is not satisfied for
        // std::sync::mpsc::Receiver, but FnOnce-by-value is fine) and hand
        // it back out after the blocking recv.
        let (rx, msg) = py.detach(move || {
            let msg = rx.recv();
            (rx, msg)
        });

        match msg {
            Ok(ChunkMsg::Data(b)) => {
                // Put the receiver back for the next iteration.
                *self.rx.lock().unwrap() = Some(rx);
                Ok(PyBytes::new(py, &b).unbind())
            }
            Ok(ChunkMsg::Eof) => {
                // Drop receiver — subsequent __next__ fast-path to StopIteration.
                Err(PyStopIteration::new_err(py.None()))
            }
            Ok(ChunkMsg::Err(e)) => Err(PyIOError::new_err(e)),
            Err(_) => {
                // Sender dropped without sending Eof — treat as EOF.
                Err(PyStopIteration::new_err(py.None()))
            }
        }
    }

    /// Read up to `n` bytes by concatenating chunks. Convenience over the
    /// iterator protocol for code that wants `read(n)` semantics. Returns
    /// `b""` at EOF. Note: may return fewer than `n` bytes if EOF arrives;
    /// may return more than `n` bytes if the buffered chunk is larger (no
    /// attempt to split frames).
    #[pyo3(signature = (n=None))]
    fn read(&self, py: Python<'_>, n: Option<usize>) -> PyResult<Py<PyBytes>> {
        let mut buf = Vec::<u8>::new();
        loop {
            match self.__next__(py) {
                Ok(chunk) => {
                    buf.extend_from_slice(chunk.bind(py).as_bytes());
                    if let Some(limit) = n {
                        if buf.len() >= limit {
                            break;
                        }
                    }
                }
                Err(e) if e.is_instance_of::<PyStopIteration>(py) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(PyBytes::new(py, &buf).unbind())
    }
}
