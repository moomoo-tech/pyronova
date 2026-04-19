//! WebSocket support — async Tokio ↔ sync Python bridge via channels.

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use pyo3::prelude::*;
use tokio_tungstenite::WebSocketStream;
use tungstenite::Message;

use crate::router::FrozenRoutes;

// ---------------------------------------------------------------------------
// Channel message type
// ---------------------------------------------------------------------------

enum WsMsg {
    Text(String),
    Binary(Vec<u8>),
}

// ---------------------------------------------------------------------------
// PyreWebSocket — Python-facing WebSocket connection object
// ---------------------------------------------------------------------------

#[pyclass]
pub(crate) struct PyreWebSocket {
    incoming_rx: std::sync::Mutex<std::sync::mpsc::Receiver<WsMsg>>,
    outgoing_tx: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<WsMsg>>>,
}

#[pymethods]
impl PyreWebSocket {
    /// Receive next text message. Returns None if connection closed.
    ///
    /// Releases the GIL while blocking on the channel so other Python
    /// threads (e.g. a second task reading from another ws, or the
    /// application's worker threads) are not frozen. Holding the GIL
    /// across a potentially unbounded channel wait is a single-threaded
    /// Python server in disguise.
    fn recv(&self, py: Python<'_>) -> Option<String> {
        py.detach(|| {
            let rx = self.incoming_rx.lock().unwrap();
            loop {
                match rx.recv().ok()? {
                    WsMsg::Text(s) => return Some(s),
                    WsMsg::Binary(_) => continue,
                }
            }
        })
    }

    /// Receive next binary message. Returns None if connection closed.
    /// Releases the GIL while waiting — see `recv` for rationale.
    fn recv_bytes(&self, py: Python<'_>) -> Option<Vec<u8>> {
        py.detach(|| {
            let rx = self.incoming_rx.lock().unwrap();
            loop {
                match rx.recv().ok()? {
                    WsMsg::Binary(b) => return Some(b),
                    WsMsg::Text(_) => continue,
                }
            }
        })
    }

    /// Receive next message as (type, data). type is "text" or "binary".
    /// Returns None if connection closed.
    fn recv_message<'py>(&self, py: Python<'py>) -> Option<(String, Py<PyAny>)> {
        // Release the GIL across the blocking recv; re-acquire to build the
        // Python-typed return value.
        let msg = py.detach(|| {
            let rx = self.incoming_rx.lock().unwrap();
            rx.recv().ok()
        })?;
        match msg {
            WsMsg::Text(s) => Some((
                "text".to_string(),
                s.into_pyobject(py).unwrap().into_any().unbind(),
            )),
            WsMsg::Binary(b) => Some((
                "binary".to_string(),
                pyo3::types::PyBytes::new(py, &b).into_any().unbind(),
            )),
        }
    }

    /// Send a text message to the client.
    fn send(&self, msg: &str) -> PyResult<()> {
        let tx = self.outgoing_tx.lock().unwrap();
        match tx.as_ref() {
            Some(tx) => tx
                .send(WsMsg::Text(msg.to_string()))
                .map_err(|_| pyo3::exceptions::PyConnectionError::new_err("WebSocket closed")),
            None => Err(pyo3::exceptions::PyConnectionError::new_err(
                "WebSocket closed",
            )),
        }
    }

    /// Send a binary message to the client.
    fn send_bytes(&self, data: Vec<u8>) -> PyResult<()> {
        let tx = self.outgoing_tx.lock().unwrap();
        match tx.as_ref() {
            Some(tx) => tx
                .send(WsMsg::Binary(data))
                .map_err(|_| pyo3::exceptions::PyConnectionError::new_err("WebSocket closed")),
            None => Err(pyo3::exceptions::PyConnectionError::new_err(
                "WebSocket closed",
            )),
        }
    }

    /// Close the WebSocket connection.
    fn close(&self) {
        let mut tx = self.outgoing_tx.lock().unwrap();
        *tx = None;
    }
}

// ---------------------------------------------------------------------------
// WebSocket upgrade detection
// ---------------------------------------------------------------------------

pub(crate) fn is_websocket_upgrade(req: &Request<Incoming>) -> bool {
    req.headers()
        .get("upgrade")
        .map(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"))
        .unwrap_or(false)
}

/// Build the 101 Switching Protocols response for WebSocket upgrade.
fn ws_upgrade_response(key: &[u8]) -> Response<Full<Bytes>> {
    let accept = tungstenite::handshake::derive_accept_key(key);

    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header("upgrade", "websocket")
        .header("connection", "Upgrade")
        .header("sec-websocket-accept", accept)
        .body(Full::new(Bytes::new()))
        .unwrap()
}

// ---------------------------------------------------------------------------
// Handle WebSocket upgrade + message pump
// ---------------------------------------------------------------------------

pub(crate) async fn handle_websocket(
    mut req: Request<Incoming>,
    routes: FrozenRoutes,
) -> Result<Response<crate::handlers::BoxBody>, hyper::Error> {
    let path = req.uri().path().to_string();

    // Look up WebSocket handler (need GIL to clone Py<PyAny>)
    let handler = Python::attach(|py| routes.ws_handlers.get(&path).map(|h| h.clone_ref(py)));

    let handler = match handler {
        Some(h) => h,
        None => {
            return Ok(crate::handlers::full_body(
                Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Full::new(Bytes::from_static(b"no websocket handler")))
                    .unwrap(),
            ));
        }
    };

    // Extract the key for the handshake
    let key = match req.headers().get("sec-websocket-key") {
        Some(k) => k.as_bytes().to_vec(),
        None => {
            return Ok(crate::handlers::full_body(
                Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(Full::new(Bytes::from_static(b"missing sec-websocket-key")))
                    .unwrap(),
            ));
        }
    };

    // Set up the upgrade
    let upgrade = hyper::upgrade::on(&mut req);

    // Spawn the WebSocket handler task
    tokio::spawn(async move {
        match upgrade.await {
            Ok(upgraded) => {
                let ws_stream = WebSocketStream::from_raw_socket(
                    hyper_util::rt::TokioIo::new(upgraded),
                    tungstenite::protocol::Role::Server,
                    None,
                )
                .await;

                run_ws_connection(ws_stream, handler).await;
            }
            Err(e) => {
                tracing::error!(target: "pyre::server", error = %e, "WebSocket upgrade error");
            }
        }
    });

    // Return the 101 response
    Ok(crate::handlers::full_body(ws_upgrade_response(&key)))
}

/// Run a WebSocket connection — bridges async Tokio with sync Python handler.
async fn run_ws_connection<S>(ws_stream: WebSocketStream<S>, handler: Py<PyAny>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut ws_sink, mut ws_source) = ws_stream.split();

    let (incoming_tx, incoming_rx) = std::sync::mpsc::channel::<WsMsg>();
    let (outgoing_tx, mut outgoing_rx) = tokio::sync::mpsc::unbounded_channel::<WsMsg>();

    let sky_ws = PyreWebSocket {
        incoming_rx: std::sync::Mutex::new(incoming_rx),
        outgoing_tx: std::sync::Mutex::new(Some(outgoing_tx)),
    };

    // Spawn a thread for the Python handler (blocks on ws.recv())
    let py_handle = std::thread::spawn(move || {
        Python::attach(|py| {
            let ws_obj = Py::new(py, sky_ws).unwrap();
            if let Err(e) = handler.call1(py, (ws_obj,)) {
                tracing::error!(target: "pyre::server", error = %e, "WebSocket handler error");
            }
        });
    });

    // Message pump: forward between WebSocket and Python channels
    loop {
        tokio::select! {
            // Client → Python (via incoming_tx)
            msg = ws_source.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if incoming_tx.send(WsMsg::Text(text.to_string())).is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Binary(data))) => {
                        if incoming_tx.send(WsMsg::Binary(data.to_vec())).is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        break;
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = ws_sink.send(Message::Pong(data)).await;
                    }
                    Some(Err(e)) => {
                        tracing::warn!(target: "pyre::server", error = %e, "WebSocket read error");
                        break;
                    }
                    _ => {} // Pong
                }
            }
            // Python → Client (via outgoing_rx)
            msg = outgoing_rx.recv() => {
                match msg {
                    Some(WsMsg::Text(text)) => {
                        if ws_sink.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Some(WsMsg::Binary(data)) => {
                        if ws_sink.send(Message::Binary(data.into())).await.is_err() {
                            break;
                        }
                    }
                    None => {
                        break;
                    }
                }
            }
        }
    }

    // Drop incoming_tx to unblock Python's ws.recv() → returns None
    drop(incoming_tx);

    // Close WebSocket
    let _ = ws_sink.close().await;

    // Wait for Python handler thread. `JoinHandle::join()` blocks, so
    // calling it directly from this async fn would pin a Tokio worker
    // thread until the Python handler finishes — a trivial DoS vector
    // when a handler hangs. Dispatch to the blocking-thread pool so
    // async workers stay free.
    let _ = tokio::task::spawn_blocking(move || py_handle.join()).await;
}
