//! Main-interpreter dispatch bridge for `gil=True` routes under TPC.
//!
//! TPC's default Phase 2 inline handler can only run sub-interpreter-safe
//! code. Legacy C extensions (numpy / pandas / torch / pydantic-core)
//! force `gil=True` on their handlers and require execution on the main
//! interpreter. This bridge makes that compatible with TPC.
//!
//! Architecture:
//!
//!   N TPC threads ──(tokio::sync::mpsc::channel, capacity=16)──▶ 1 main-interp thread
//!                            ↑                                          │
//!                            │ bounded                                  │ Python::attach
//!                       try_send → 503 on full                          │ (main GIL)
//!                            │                                          │
//!                            ◀─────────(tokio::sync::oneshot)─────────── ◁
//!                              response via oneshot per request
//!
//! Throughput contract: the bridge is a *cold path*. CPython's GIL caps
//! the main-interp thread at the single-thread rate of whatever the
//! handler's work is (typically ≤ 10k rps for numpy-class workloads,
//! often much less). The channel capacity is deliberately small (16) so
//! that when the bridge is saturated, TPC threads 503 *fast* rather
//! than accumulating memory on a queue that services slower than the
//! fleet's inbound rate. The rest of the fleet (pure sub-interp routes)
//! keeps running at TPC speed — the GIL-bound slowdown stays contained.
//!
//! `dispatch_one` reuses the existing `call_handler_with_hooks` from
//! `handlers.rs` — same semantics as the non-TPC GIL path: before hooks
//! → handler → after hooks → response extraction. Coroutines and
//! streams fall through the same logic.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::OnceLock;

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};

use crate::handlers::{call_handler_with_hooks, HandlerResult, StreamInfo};
use crate::router::FrozenRoutes;
use crate::types::{LazyHeaders, PyronovaRequest, ResponseData};

/// Bridge reply can be either a buffered response or a streaming one.
/// Stream handlers (SSE, chunked upload replies, long-poll) need to
/// hand an mpsc::Receiver back to the TPC thread so the hyper body
/// writer can drive it. The oneshot channel is Send + one-shot, and
/// tokio's mpsc::Receiver is Send, so transferring StreamInfo
/// across the main-interp → TPC thread boundary is safe.
pub(crate) enum BridgeResponse {
    Resp(ResponseData),
    Stream(StreamInfo),
}

/// Work request for the main-interp bridge. Carries everything a GIL
/// handler needs plus the oneshot reply channel.
pub(crate) struct GilWorkItem {
    pub method: Arc<str>,
    pub path: Arc<str>,
    pub params: Vec<(String, String)>,
    pub query: String,
    pub body: Bytes,
    pub headers: HashMap<String, String>,
    pub client_ip: IpAddr,
    pub handler_idx: usize,
    /// Body-stream receiver for `stream=True` routes. The feeder task
    /// running on the TPC thread's LocalSet pushes body frames into
    /// this receiver; the bridge thread's handler pulls them. Buffered
    /// routes share the [`crate::python::body_stream::empty_body_stream_rx`]
    /// singleton — `body` carries the collected bytes there.
    pub body_stream_rx: crate::python::body_stream::BodyStreamRx,
    pub response_tx: oneshot::Sender<Result<BridgeResponse, String>>,
}

/// Handle returned to callers. Cheap to Arc-share across all TPC
/// threads.
pub(crate) struct MainInterpBridge {
    tx: mpsc::Sender<GilWorkItem>,
    // Join handle deliberately dropped-detached. The bridge thread
    // exits when the mpsc Sender count hits zero (server shutdown
    // dropping Arcs). At that point main interp cleanup runs naturally.
}

impl MainInterpBridge {
    /// Spawn the main-interp bridge thread with an mpsc channel of the
    /// given capacity. Returns an Arc-safe handle for cloning to every
    /// TPC thread's dispatch path. Channel capacity should be small
    /// (default 16) — see module doc for why.
    pub(crate) fn spawn(routes: FrozenRoutes, capacity: usize) -> Arc<Self> {
        let (tx, mut rx) = mpsc::channel::<GilWorkItem>(capacity);
        let routes_owned = routes;

        // The bridge thread intentionally does NOT spin up a tokio
        // runtime. Python handlers on the main interp may call
        // `req.stream.drain_count()` which internally calls
        // `Receiver::blocking_recv` on the body-stream mpsc — and
        // blocking_recv panics if invoked from an async runtime
        // context. A plain std::thread with `rx.blocking_recv()` on
        // the work channel keeps us outside of any runtime, so the
        // downstream Python-side blocking_recv works correctly.
        std::thread::Builder::new()
            .name("pyronova-main-bridge".into())
            .spawn(move || {
                loop {
                    // `mpsc::Receiver::blocking_recv()` is the non-
                    // runtime-bound counterpart to `.recv().await`.
                    // Returns None when all Senders drop (server
                    // shutdown), which is our exit signal.
                    let item = match rx.blocking_recv() {
                        Some(i) => i,
                        None => break,
                    };
                    dispatch_one(&routes_owned, item);
                }
                tracing::info!(
                    target: "pyronova::server",
                    "main-interp bridge thread exiting (channel closed)"
                );
            })
            .expect("spawn main bridge thread");

        Arc::new(MainInterpBridge { tx })
    }

    /// Non-blocking dispatch. Returns Err with the original work item
    /// when the channel is full (caller should respond 503) or when
    /// the bridge thread has exited (caller should respond 500).
    pub(crate) fn try_dispatch(
        &self,
        item: GilWorkItem,
    ) -> Result<(), (GilWorkItem, TryDispatchError)> {
        match self.tx.try_send(item) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(item)) => {
                Err((item, TryDispatchError::Full))
            }
            Err(mpsc::error::TrySendError::Closed(item)) => {
                Err((item, TryDispatchError::Closed))
            }
        }
    }
}

pub(crate) enum TryDispatchError {
    Full,
    Closed,
}

fn dispatch_one(routes: &FrozenRoutes, item: GilWorkItem) {
    let GilWorkItem {
        method,
        path,
        params,
        query,
        body,
        headers,
        client_ip,
        handler_idx,
        body_stream_rx,
        response_tx,
    } = item;

    // Fast bailout if the caller's TPC task has already given up
    // waiting (client disconnect, upstream timeout). Don't burn main
    // interp's single GIL on a request nobody is going to read.
    if response_tx.is_closed() {
        return;
    }

    // Reconstruct the pyo3 PyronovaRequest from raw parts. Same shape
    // as what `handle_request` builds in non-TPC GIL mode, so the
    // downstream call_handler_with_hooks path is byte-for-byte the
    // same behavior.
    let sky_req = PyronovaRequest {
        method,
        path,
        params,
        query,
        headers_source: LazyHeaders::Converted(headers),
        headers_cache: OnceLock::new(),
        query_cache: OnceLock::new(),
        client_ip_addr: client_ip,
        body_bytes: body,
        body_stream_rx,
    };

    let result = call_handler_with_hooks(routes.clone(), handler_idx, sky_req);
    let resp = match result {
        HandlerResult::PyronovaResponse(Ok(rd)) => Ok(BridgeResponse::Resp(rd)),
        HandlerResult::PyronovaResponse(Err(e)) => Err(e),
        HandlerResult::PyronovaStream(info) => Ok(BridgeResponse::Stream(info)),
    };
    let _ = response_tx.send(resp);
}
