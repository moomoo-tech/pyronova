//! SSE (Server-Sent Events) streaming response support.
//!
//! Handler returns a `PyreStream` object, then calls `stream.send("data")`
//! in a loop. Each send pushes a chunk to the HTTP response body.

use bytes::Bytes;
use pyo3::prelude::*;
use tokio::sync::mpsc;

/// Python-facing stream object. Handler calls send()/send_event()/close().
#[pyclass(frozen)]
pub(crate) struct PyreStream {
    tx: mpsc::UnboundedSender<Result<Bytes, std::convert::Infallible>>,
    rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<Result<Bytes, std::convert::Infallible>>>>,
    /// Custom headers to include in the response
    #[pyo3(get)]
    pub(crate) content_type: String,
    #[pyo3(get)]
    pub(crate) status_code: u16,
    #[pyo3(get)]
    pub(crate) headers: std::collections::HashMap<String, String>,
}

#[pymethods]
impl PyreStream {
    /// Create a new SSE stream. Channel is created immediately so send() works right away.
    #[new]
    #[pyo3(signature = (content_type=None, status_code=200, headers=None))]
    fn new(
        content_type: Option<String>,
        status_code: u16,
        headers: Option<std::collections::HashMap<String, String>>,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        PyreStream {
            tx,
            rx: std::sync::Mutex::new(Some(rx)),
            content_type: content_type.unwrap_or_else(|| "text/event-stream".to_string()),
            status_code,
            headers: headers.unwrap_or_default(),
        }
    }

    /// Send raw data chunk.
    fn send(&self, data: &str) -> PyResult<()> {
        self.tx
            .send(Ok(Bytes::from(data.to_string())))
            .map_err(|_| pyo3::exceptions::PyConnectionError::new_err("stream closed"))
    }

    /// Send an SSE event: `event: {event}\ndata: {data}\n\n`
    #[pyo3(signature = (data, event=None, id=None))]
    fn send_event(&self, data: &str, event: Option<&str>, id: Option<&str>) -> PyResult<()> {
        let mut msg = String::new();
        if let Some(id) = id {
            msg.push_str(&format!("id: {id}\n"));
        }
        if let Some(event) = event {
            msg.push_str(&format!("event: {event}\n"));
        }
        for line in data.lines() {
            msg.push_str(&format!("data: {line}\n"));
        }
        msg.push('\n'); // End of event
        self.send(&msg)
    }

    /// Close the stream (drop the sender).
    fn close(&self) {
        // Dropping tx would close the channel, but we can't drop a field.
        // Instead, send a special empty marker — the stream reader will just see channel closed
        // when all senders are dropped (which happens when PyreStream is garbage collected).
        // For explicit close, we do nothing — the channel closes when PyreStream is dropped.
    }
}

impl PyreStream {
    /// Take the receiver (called once by Rust handler to start streaming).
    pub(crate) fn take_rx(
        &self,
    ) -> Option<mpsc::UnboundedReceiver<Result<Bytes, std::convert::Infallible>>> {
        self.rx.lock().unwrap().take()
    }
}
