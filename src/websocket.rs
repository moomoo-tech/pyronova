//! WebSocket support — async Tokio ↔ sync Python bridge via channels.

use std::sync::Arc;

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
// SkyWebSocket — Python-facing WebSocket connection object
// ---------------------------------------------------------------------------

#[pyclass]
pub(crate) struct SkyWebSocket {
    /// Receive messages from client (async → sync bridge)
    incoming_rx: std::sync::Mutex<std::sync::mpsc::Receiver<String>>,
    /// Send messages to client (sync → async bridge)
    outgoing_tx: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>,
}

#[pymethods]
impl SkyWebSocket {
    /// Receive next message from client. Returns None if connection closed.
    fn recv(&self) -> Option<String> {
        let rx = self.incoming_rx.lock().unwrap();
        rx.recv().ok()
    }

    /// Send a text message to the client.
    fn send(&self, msg: &str) -> PyResult<()> {
        let tx = self.outgoing_tx.lock().unwrap();
        match tx.as_ref() {
            Some(tx) => tx.send(msg.to_string()).map_err(|_| {
                pyo3::exceptions::PyConnectionError::new_err("WebSocket closed")
            }),
            None => Err(pyo3::exceptions::PyConnectionError::new_err(
                "WebSocket closed",
            )),
        }
    }

    /// Close the WebSocket connection.
    fn close(&self) {
        let mut tx = self.outgoing_tx.lock().unwrap();
        *tx = None; // Drop the sender, which signals the async side to close
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
    let handler = Python::attach(|py| {
        routes.ws_handlers.get(&path).map(|h| h.clone_ref(py))
    });

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
                eprintln!("WebSocket upgrade error: {e}");
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

    // Channels for async ↔ sync bridge
    // incoming: async task → Python thread (client messages)
    // outgoing: Python thread → async task (server messages)
    let (incoming_tx, incoming_rx) = std::sync::mpsc::channel::<String>();
    let (outgoing_tx, mut outgoing_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    // Create the Python WebSocket object
    let sky_ws = SkyWebSocket {
        incoming_rx: std::sync::Mutex::new(incoming_rx),
        outgoing_tx: std::sync::Mutex::new(Some(outgoing_tx)),
    };

    // Spawn a thread for the Python handler (blocks on ws.recv())
    let py_handle = std::thread::spawn(move || {
        Python::attach(|py| {
            let ws_obj = Py::new(py, sky_ws).unwrap();
            if let Err(e) = handler.call1(py, (ws_obj,)) {
                eprintln!("WebSocket handler error: {e}");
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
                        if incoming_tx.send(text.to_string()).is_err() {
                            break; // Python handler exited
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        break; // Client closed
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = ws_sink.send(Message::Pong(data)).await;
                    }
                    Some(Err(e)) => {
                        eprintln!("WebSocket read error: {e}");
                        break;
                    }
                    _ => {} // Binary, Pong
                }
            }
            // Python → Client (via outgoing_rx)
            msg = outgoing_rx.recv() => {
                match msg {
                    Some(text) => {
                        if ws_sink.send(Message::Text(text.into())).await.is_err() {
                            break; // Client disconnected
                        }
                    }
                    None => {
                        break; // Python called close()
                    }
                }
            }
        }
    }

    // Drop incoming_tx to unblock Python's ws.recv() → returns None
    drop(incoming_tx);

    // Close WebSocket
    let _ = ws_sink.close().await;

    // Wait for Python handler thread
    let _ = py_handle.join();
}
