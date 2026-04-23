use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::{Request, Response, StatusCode};
use pyo3::prelude::*;
use pyo3::types::PyString;

use crate::interp;
use crate::response::{
    build_response, error_response, extract_response_data, gateway_timeout_response,
    not_found_response, overloaded_response, payload_too_large_response,
};
use crate::router::FrozenRoutes;
use crate::static_fs::try_static_file;
use crate::stream::PyronovaStream;
use crate::types::{extract_headers, PyronovaRequest, PyronovaResponse, ResponseData};

type SharedPool = Arc<interp::InterpreterPool>;

// ---------------------------------------------------------------------------
// TPC inline handler — Phase 2
// ---------------------------------------------------------------------------
//
// Replaces the `pool.submit() + oneshot.await` cross-thread hop with a
// direct synchronous call into the TPC thread's own sub-interpreter.
// Zero channels, zero cross-thread wakes. Kernel SO_REUSEPORT handles
// load balance at the TCP layer; within a TPC thread, requests are
// served strictly sequentially (slow handler ⇒ thread stalls, kernel
// reshards new traffic to peer threads — by design).
//
// Startup enforces TPC constraints: all routes must be sync + non-GIL
// + non-streaming. A route with `gil=True`, `async def`, or
// `stream=True` makes PyronovaApp::run_tpc_subinterp refuse to start
// with an explicit error (see src/app.rs::run_tpc_subinterp).
//
// Non-Send future by design: Rc<RefCell<SubInterpreterWorker>> pins
// the worker to the calling OS thread, which is exactly what we need
// for a current_thread runtime + LocalSet. Attempting to spawn this
// future via `tokio::spawn` would fail to compile — a feature, not a
// bug.

pub(crate) async fn handle_request_tpc_inline(
    req: Request<Incoming>,
    routes: FrozenRoutes,
    worker: std::rc::Rc<std::cell::RefCell<interp::SubInterpreterWorker>>,
    client_ip_addr: std::net::IpAddr,
) -> Result<Response<BoxBody>, hyper::Error> {
    // gRPC short-circuit — fully handled in Rust, doesn't touch the sub-interp.
    if crate::grpc::is_grpc_request(&req) {
        return crate::grpc::handle_grpc(req).await;
    }
    crate::monitor::TOTAL_REQUESTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let start = std::time::Instant::now();
    let method: Arc<str> = Arc::from(req.method().as_str());
    let uri = req.uri().clone();
    let path: Arc<str> = Arc::from(uri.path());
    let query = uri.query().unwrap_or("").to_string();

    // Fast-path: pre-built response. Zero Python, served from Rust.
    if !routes.fast_responses.is_empty() {
        if let Some(fr) = routes
            .fast_responses
            .get(&(method.to_string(), path.to_string()))
        {
            return Ok(full_body(build_fast_response(
                fr,
                routes.cors_config.as_ref(),
            )));
        }
    }

    let raw_headers = req.headers().clone();
    let accept_encoding = raw_headers
        .get(hyper::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body_obj = req.into_body();

    // Router lookup.
    let lookup = routes
        .routers
        .get(method.as_ref())
        .and_then(|r| r.at(&path).ok())
        .map(|m| {
            let params: Vec<(String, String)> = m
                .params
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            (*m.value, params)
        });

    // Static files — only on GET/HEAD miss.
    if lookup.is_none() && (method.as_ref() == "GET" || method.as_ref() == "HEAD") {
        if let Some(resp) = try_static_file(&path, &routes.static_dirs).await {
            let mut r = full_body(resp);
            apply_cors(&mut r, routes.cors_config.as_ref());
            return Ok(r);
        }
    }

    let (handler_idx, params) = match lookup {
        Some(v) => v,
        None => {
            let mut r = full_body(not_found_response());
            apply_cors(&mut r, routes.cors_config.as_ref());
            return Ok(r);
        }
    };

    // Body collect (no streaming in TPC Phase 2 — enforced at startup).
    let body_bytes = match collect_body_bounded(body_obj).await {
        Ok(b) => b,
        Err(resp) => {
            let mut r = resp;
            apply_cors(&mut r, routes.cors_config.as_ref());
            return Ok(r);
        }
    };

    let headers = extract_headers(&raw_headers);
    let method_log = Arc::clone(&method);
    let path_log = Arc::clone(&path);
    let handler_name = routes.handler_names[handler_idx].clone();

    // The inline dispatch: acquire the TPC thread's sub-interp GIL,
    // run the handler, release. Blocks the thread (which is the point —
    // peer TPC threads continue serving via SO_REUSEPORT). Non-Send
    // because Rc<RefCell<_>> and *mut PyThreadState cross no await.
    let result = {
        let mut worker_ref = worker.borrow_mut();
        let tstate_cell = std::cell::Cell::new(worker_ref.tstate);
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            let _guard =
                interp::SubInterpGilGuard::acquire(tstate_cell.get(), &tstate_cell);
            worker_ref.call_handler(
                &handler_name,
                &routes.before_hook_names,
                &routes.after_hook_names,
                &method,
                &path,
                &params,
                &query,
                &body_bytes,
                &headers,
                &client_ip_addr.to_string(),
            )
        }));
        worker_ref.tstate = tstate_cell.get();
        match res {
            Ok(r) => r,
            Err(_) => Err("internal error: TPC handler panic".to_string()),
        }
    };

    let mut http_resp = match result {
        Ok(mut resp) => {
            let ct_owned: String = resp.content_type.clone().unwrap_or_else(|| {
                if resp.is_json
                    || resp.body.starts_with(b"{")
                    || resp.body.starts_with(b"[")
                {
                    "application/json".to_string()
                } else {
                    "text/plain; charset=utf-8".to_string()
                }
            });
            crate::compression::maybe_compress_subinterp(
                &mut resp.body,
                &ct_owned,
                &mut resp.headers,
                &accept_encoding,
            );
            let status = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::OK);
            let mut builder = Response::builder()
                .status(status)
                .header("content-type", &ct_owned)
                .header("server", crate::response::SERVER_HEADER);
            for (k, v) in &resp.headers {
                builder = builder.header(k.as_str(), v.as_str());
            }
            match builder.body(Full::new(Bytes::from(resp.body))) {
                Ok(r) => full_body(r),
                Err(_) => full_body(error_response("invalid response headers")),
            }
        }
        Err(e) => full_body(error_response(&e)),
    };
    apply_cors(&mut http_resp, routes.cors_config.as_ref());
    let latency_us = start.elapsed().as_micros() as u64;
    let status = http_resp.status().as_u16();
    if routes.request_logging {
        tracing::info!(
            target: "pyronova::access",
            method = %method_log,
            path = %path_log,
            status,
            latency_us,
            mode = "tpc-inline",
            "PyronovaRequest handled"
        );
    }
    Ok(http_resp)
}

/// Bounded body collection. Returns Err with a ready-made error response
/// on oversize/stream errors.
async fn collect_body_bounded(
    body: Incoming,
) -> Result<Vec<u8>, Response<BoxBody>> {
    use http_body_util::Limited;
    let max = max_body_size();
    let limited = Limited::new(body, max);
    match limited.collect().await {
        Ok(c) => Ok(c.to_bytes().to_vec()),
        Err(e) => {
            if e.downcast_ref::<http_body_util::LengthLimitError>().is_some() {
                Err(full_body(payload_too_large_response()))
            } else {
                Err(full_body(error_response("body read failed")))
            }
        }
    }
}

/// Default max request body size (10 MB). Configurable via `app.max_body_size`.
const DEFAULT_MAX_BODY_SIZE: usize = 10 * 1024 * 1024;

/// Global max body size — set once at startup, read on every request (lock-free).
static MAX_BODY_SIZE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(DEFAULT_MAX_BODY_SIZE);

pub(crate) fn set_max_body_size(size: usize) {
    MAX_BODY_SIZE.store(size, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn max_body_size() -> usize {
    MAX_BODY_SIZE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Result from handler: either a normal response or a stream.
enum HandlerResult {
    PyronovaResponse(Result<ResponseData, String>),
    PyronovaStream(StreamInfo),
}

struct StreamInfo {
    rx: tokio::sync::mpsc::Receiver<Result<Bytes, std::convert::Infallible>>,
    content_type: String,
    status: u16,
    headers: HashMap<String, String>,
}

/// If `obj` is a coroutine (from `async def`), execute it via a thread-local
/// persistent asyncio event loop. Otherwise return it unchanged.
///
/// Uses thread_local to cache event loop per spawn_blocking thread —
/// avoids asyncio.run() overhead of creating/destroying loop per request.
fn resolve_coroutine(py: Python<'_>, obj: Py<PyAny>) -> Result<Py<PyAny>, String> {
    use std::cell::RefCell;

    /// RAII wrapper that calls `loop.close()` via GIL when the thread dies.
    /// Prevents FD / task leaks from orphaned asyncio event loops.
    ///
    /// Safety: guards against Py_Finalize race — if the Python interpreter
    /// is already shutting down when this thread exits, we skip the FFI call
    /// (leak the loop object) rather than segfault on invalid thread state.
    struct LoopGuard(Option<Py<PyAny>>);
    impl Drop for LoopGuard {
        fn drop(&mut self) {
            if let Some(loop_obj) = self.0.take() {
                // Check that the Python interpreter is still alive.
                // During process shutdown, Tokio's blocking thread pool may
                // tear down threads AFTER Py_Finalize has run, making
                // Python::attach() UB (use-after-free on global interpreter).
                if unsafe { pyo3::ffi::Py_IsInitialized() } != 0 {
                    Python::attach(|py| {
                        let _ = loop_obj.call_method0(py, "close");
                    });
                    // loop_obj's Py<PyAny>::Drop fires here under a live
                    // interp → benign refcount decrement.
                } else {
                    // Interp is gone. Py<PyAny>::Drop would try to
                    // Py_DECREF on a finalized interpreter → segfault.
                    // Physically leak the pointer — at this point the
                    // whole process is exiting, so the OS reclaims it.
                    std::mem::forget(loop_obj);
                }
            }
        }
    }

    thread_local! {
        static LOOP: RefCell<LoopGuard> = const { RefCell::new(LoopGuard(None)) };
    }

    let bound = obj.bind(py);
    // Awaitable detection via C-level type slot probe.
    //
    // The canonical Python way — `inspect.isawaitable(obj)` — dispatches
    // through the inspect module, costing ~μs per call (import + attr
    // lookup + method call + refcount dance). On a hot per-request
    // middleware path at 400k rps that's measurable — Pyronova v1.4.5
    // bench saw ~5% throughput loss from this single check.
    //
    // Instead, read the type's `tp_as_async->am_await` slot directly.
    // Any awaitable (native coroutine, asyncio.Task, asyncio.Future,
    // user classes implementing __await__ via PyType_FromSpec with
    // Py_am_await) has am_await populated. Cost: one pointer chase +
    // one null check. Nanoseconds, L1-resident.
    let is_awaitable = unsafe {
        let ptr = bound.as_ptr();
        if pyo3::ffi::PyCoro_CheckExact(ptr) == 1 {
            true
        } else {
            let tp = pyo3::ffi::Py_TYPE(ptr);
            if tp.is_null() {
                false
            } else {
                let async_slots = (*tp).tp_as_async;
                !async_slots.is_null() && (*async_slots).am_await.is_some()
            }
        }
    };
    if !is_awaitable {
        return Ok(obj);
    }

    LOOP.with(|tl| {
        let mut guard = tl.borrow_mut();

        if guard.0.is_none() {
            let asyncio = py
                .import("asyncio")
                .map_err(|e| format!("import asyncio: {e}"))?;
            let new_loop = asyncio
                .call_method0("new_event_loop")
                .map_err(|e| format!("new_event_loop: {e}"))?;
            let _ = asyncio.call_method1("set_event_loop", (&new_loop,));
            guard.0 = Some(new_loop.unbind());
        }

        let event_loop = guard.0.as_ref().unwrap().bind(py);
        let result = event_loop
            .call_method1("run_until_complete", (bound,))
            .map_err(|e| format!("run_until_complete error: {e}"))?;
        Ok(result.unbind())
    })
}

// ---------------------------------------------------------------------------
// GIL mode handler
// ---------------------------------------------------------------------------

pub(crate) type BoxBody = http_body_util::combinators::BoxBody<Bytes, hyper::Error>;

/// Inject CORS headers into any response (normal or error).
///
/// Per W3C CORS spec, Access-Control-Allow-Credentials and
/// Access-Control-Expose-Headers must be present on actual responses
/// (GET/POST/etc.), not only OPTIONS preflight.
fn apply_cors(resp: &mut Response<BoxBody>, cors: Option<&crate::router::CorsConfig>) {
    let Some(cfg) = cors else { return };
    let headers = resp.headers_mut();
    // insert (not append) to avoid duplicates
    if let Ok(v) = cfg.origin.parse() {
        headers.insert("access-control-allow-origin", v);
    }
    if let Ok(v) = cfg.methods.parse() {
        headers.insert("access-control-allow-methods", v);
    }
    if let Ok(v) = cfg.headers.parse() {
        headers.insert("access-control-allow-headers", v);
    }
    if cfg.allow_credentials {
        headers.insert("access-control-allow-credentials", "true".parse().unwrap());
    }
    if let Some(expose) = cfg.expose_headers.as_ref() {
        if let Ok(v) = expose.parse() {
            headers.insert("access-control-expose-headers", v);
        }
    }
}

pub(crate) fn full_body(resp: Response<Full<Bytes>>) -> Response<BoxBody> {
    resp.map(|b| b.map_err(|_| unreachable!()).boxed())
}

/// Build a hyper response from a `FastResponse` — used for the fast-path
/// route table (`app.add_fast_response`). No Python touched; just copies
/// the pre-built Bytes + headers into a Response<Full<Bytes>>.
fn build_fast_response(
    fr: &crate::router::FastResponse,
    cors: Option<&crate::router::CorsConfig>,
) -> Response<Full<Bytes>> {
    let status = StatusCode::from_u16(fr.status).unwrap_or(StatusCode::OK);
    let mut builder = Response::builder()
        .status(status)
        .header("content-type", &fr.content_type)
        .header("server", crate::response::SERVER_HEADER);
    for (k, v) in &fr.headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    // Inline a subset of apply_cors so we don't allocate a BoxBody just
    // to mutate and reboxed. The fast path is latency-sensitive.
    if let Some(cfg) = cors {
        if let Ok(v) = cfg.origin.parse::<hyper::header::HeaderValue>() {
            builder = builder.header("access-control-allow-origin", v);
        }
        if let Ok(v) = cfg.methods.parse::<hyper::header::HeaderValue>() {
            builder = builder.header("access-control-allow-methods", v);
        }
    }
    builder
        .body(Full::new(fr.body.clone()))
        .unwrap_or_else(|_| error_response("invalid fast response"))
}

/// Feeder task for `stream=True` routes. Reads one hyper body frame at a
/// time and pushes each data chunk into the `PyronovaBodyStream`'s mpsc channel.
/// Enforces `max_size` as a running total (defense against malicious
/// unbounded uploads) and per-frame read timeout (Slowloris defense, same
/// 30 s budget as the buffered path).
async fn stream_body_feeder(
    body: Incoming,
    tx: tokio::sync::mpsc::Sender<crate::body_stream::ChunkMsg>,
    max_size: usize,
) {
    use crate::body_stream::ChunkMsg;
    use hyper::body::Body;
    let mut body = body;
    let mut total: usize = 0;
    loop {
        let frame_res = match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            std::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx)),
        )
        .await
        {
            Ok(Some(Ok(f))) => f,
            Ok(Some(Err(e))) => {
                let _ = tx.send(ChunkMsg::Err(format!("body read: {e}"))).await;
                return;
            }
            Ok(None) => {
                let _ = tx.send(ChunkMsg::Eof).await;
                return;
            }
            Err(_) => {
                let _ = tx
                    .send(ChunkMsg::Err("body read timeout (30s)".into()))
                    .await;
                return;
            }
        };
        if let Ok(chunk) = frame_res.into_data() {
            total = total.saturating_add(chunk.len());
            if total > max_size {
                let _ = tx
                    .send(ChunkMsg::Err(format!(
                        "body exceeds max_body_size ({max_size} bytes)"
                    )))
                    .await;
                return;
            }
            // `.send().await` propagates backpressure all the way back to
            // hyper's poll_frame: a slow Python consumer blocks the feeder,
            // which blocks the next poll, which closes the TCP receive
            // window so the client slows down on the wire. The previous
            // `std::sync::mpsc::Sender::send()` was unbounded — a fast
            // network peer could dump gigabytes into RAM before the Python
            // handler consumed the first chunk. See body_stream.rs module
            // doc for the bound (CHANNEL_CAPACITY = 8 frames in flight).
            if tx.send(ChunkMsg::Data(chunk)).await.is_err() {
                // Handler dropped the stream — no one to receive further chunks.
                return;
            }
        }
        // Trailer / metadata frames are ignored for body streaming.
    }
}

pub(crate) async fn handle_request(
    req: Request<Incoming>,
    routes: FrozenRoutes,
    client_ip_addr: std::net::IpAddr,
) -> Result<Response<BoxBody>, hyper::Error> {
    // gRPC short-circuit: if the request looks like gRPC
    // (`application/grpc*` content-type on POST), it goes to the
    // hand-rolled unary dispatcher instead of the normal routing
    // pipeline. gRPC responses need HTTP/2 trailers with `grpc-status`,
    // which the regular Response<Full<Bytes>> path doesn't model.
    if crate::grpc::is_grpc_request(&req) {
        return crate::grpc::handle_grpc(req).await;
    }
    crate::monitor::TOTAL_REQUESTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let start = std::time::Instant::now();
    let method: Arc<str> = Arc::from(req.method().as_str());
    let uri = req.uri().clone();
    let path: Arc<str> = Arc::from(uri.path());
    let query = uri.query().unwrap_or("").to_string();

    // Fast-path: pre-built response registered via app.add_fast_response().
    // Served entirely from Rust — no Python call, no GIL wait, no body read.
    // Checked before headers/body because for fast routes none of that
    // matters; we have the exact bytes + status + content-type already.
    //
    // The `is_empty()` guard is load-bearing: the lookup builds two
    // `String` allocations for the hashmap key (method + path), which
    // at 500k rps is 1M malloc/s of pure waste for apps that don't
    // register any fast routes. Skip the allocs entirely when there
    // are no fast responses to match against — the `.is_empty()` check
    // is a single pointer compare.
    if !routes.fast_responses.is_empty() {
        if let Some(fr) = routes
            .fast_responses
            .get(&(method.to_string(), path.to_string()))
        {
            return Ok(full_body(build_fast_response(
                fr,
                routes.cors_config.as_ref(),
            )));
        }
    }

    // Lazy headers: store raw HeaderMap, convert only if Python accesses req.headers.
    let raw_headers = req.headers().clone();
    // Capture Accept-Encoding before raw_headers is moved into the PyronovaRequest.
    let accept_encoding = raw_headers
        .get(hyper::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // Take ownership of the body before the route lookup — we may either
    // collect it synchronously (buffered path) or spawn a feeder task
    // (streaming path) based on the matched route's `stream` flag.
    let body_obj = req.into_body();

    let lookup = routes.lookup(&method, &path);
    let has_fallback = routes.fallback_handler.is_some();

    if lookup.is_none() && (method.as_ref() == "GET" || method.as_ref() == "HEAD") {
        if let Some(resp) = try_static_file(&path, &routes.static_dirs).await {
            // Static file hit: apply CORS before returning. Previously
            // this early-return path skipped `apply_cors` at the function
            // bottom, so CORS-configured apps served static assets
            // without the allow-origin / allow-methods headers.
            let mut r = full_body(resp);
            apply_cors(&mut r, routes.cors_config.as_ref());
            return Ok(r);
        }
    }

    let (handler_idx, params) = match lookup {
        Some(v) => v,
        None if has_fallback => (usize::MAX, Vec::new()),
        // 404 must still carry CORS headers — browsers running an
        // OPTIONS preflight against an unknown path would otherwise
        // block the real cross-origin request with a generic "CORS
        // policy" error that's painful to debug. The normal dispatch
        // path applies CORS at function-exit; we replicate that here.
        None => {
            let mut r = full_body(not_found_response());
            apply_cors(&mut r, routes.cors_config.as_ref());
            return Ok(r);
        }
    };

    let is_stream_route =
        handler_idx != usize::MAX && routes.is_stream.get(handler_idx).copied().unwrap_or(false);

    let (body_bytes, body_stream_rx) = if is_stream_route {
        // Streaming path: spawn a feeder that pushes body frames into a
        // channel. The handler takes the receiver out via `req.stream`
        // on first access.
        let (tx, rx) = tokio::sync::mpsc::channel::<crate::body_stream::ChunkMsg>(
            crate::body_stream::CHANNEL_CAPACITY,
        );
        let cap = max_body_size();
        tokio::spawn(stream_body_feeder(body_obj, tx, cap));
        (Bytes::new(), Arc::new(std::sync::Mutex::new(Some(rx))))
    } else {
        // Buffered path (default): collect the whole body with size + time limits.
        use http_body_util::Limited;
        let limited = Limited::new(body_obj, max_body_size());
        let bytes =
            match tokio::time::timeout(std::time::Duration::from_secs(30), limited.collect()).await
            {
                Ok(Ok(c)) => c.to_bytes(),
                Ok(Err(_)) => {
                    let mut r = full_body(payload_too_large_response());
                    apply_cors(&mut r, routes.cors_config.as_ref());
                    return Ok(r);
                }
                Err(_) => {
                    let mut r = full_body(crate::response::gateway_timeout_response());
                    apply_cors(&mut r, routes.cors_config.as_ref());
                    return Ok(r);
                }
            };
        (bytes, Arc::new(std::sync::Mutex::new(None)))
    };

    let method_log = Arc::clone(&method);
    let path_log = Arc::clone(&path);
    let sky_req = PyronovaRequest {
        method,
        path,
        params,
        query,
        headers_source: crate::types::LazyHeaders::Raw(raw_headers),
        headers_cache: std::sync::OnceLock::new(),
        client_ip_addr,
        body_bytes,
        body_stream_rx,
        query_cache: std::sync::OnceLock::new(),
    };

    // spawn_blocking: prevent GIL acquisition from starving Tokio workers
    let routes_ref = Arc::clone(&routes);
    let handler_result = tokio::task::spawn_blocking(move || {
        call_handler_with_hooks(routes_ref, handler_idx, sky_req)
    })
    .await
    .unwrap_or_else(|_| {
        HandlerResult::PyronovaResponse(Err("handler thread panicked".to_string()))
    });

    let mut resp = match handler_result {
        HandlerResult::PyronovaResponse(mut result) => {
            if let Ok(data) = result.as_mut() {
                crate::compression::maybe_compress(data, &accept_encoding);
            }
            full_body(build_response(result)?)
        }
        HandlerResult::PyronovaStream(info) => build_stream_response(info),
    };

    apply_cors(&mut resp, routes.cors_config.as_ref());
    let latency_us = start.elapsed().as_micros() as u64;
    let status = resp.status().as_u16();
    if routes.request_logging {
        tracing::info!(
            target: "pyronova::access",
            method = %method_log,
            path = %path_log,
            status,
            latency_us,
            mode = "gil",
            "PyronovaRequest handled"
        );
    }
    Ok(resp)
}

// ---------------------------------------------------------------------------
// Shared: call handler with full middleware chain (runs in blocking thread)
// ---------------------------------------------------------------------------

fn call_handler_with_hooks(
    routes: FrozenRoutes,
    handler_idx: usize,
    sky_req: PyronovaRequest,
) -> HandlerResult {
    use std::sync::atomic::Ordering::Relaxed;

    // Track GIL queue: +1 before acquiring, -1 after acquiring
    crate::monitor::GIL_QUEUE_LENGTH.fetch_add(1, Relaxed);

    // Passive GIL contention measurement: record the wall-clock time spent
    // waiting to acquire the GIL. This replaces the active watchdog probe —
    // measures real request latency instead of artificial contention.
    let gil_wait_start = std::time::Instant::now();

    Python::attach(|py| {
        crate::monitor::GIL_QUEUE_LENGTH.fetch_sub(1, Relaxed);
        crate::monitor::record_gil_wait(gil_wait_start.elapsed().as_micros() as u64);
        let hold_start = std::time::Instant::now();

        // FrozenRoutes: zero-cost iteration — direct Arc<RouteTable> reference.
        // No Vec collect, no clone_ref. Just borrow from the Arc.
        let before_hooks = &routes.before_hooks;
        let after_hooks = &routes.after_hooks;

        let handler = if handler_idx == usize::MAX {
            routes.fallback_handler.as_ref().unwrap()
        } else {
            &routes.handlers[handler_idx]
        };

        // before_request hooks
        //
        // Drive the hook return through resolve_coroutine — an `async def`
        // middleware returns a coroutine that must be awaited, not treated
        // as a live response. Without this, `!bound.is_none()` was true
        // for the coroutine object and the framework returned
        // "<coroutine object ...>" as a 200 body.
        for hook in before_hooks {
            match hook.call1(py, (sky_req.clone(),)) {
                Ok(result) => {
                    let result = match resolve_coroutine(py, result) {
                        Ok(r) => r,
                        Err(e) => {
                            return HandlerResult::PyronovaResponse(Err(format!(
                                "before_request hook error: {e}"
                            )))
                        }
                    };
                    let bound = result.bind(py);
                    if !bound.is_none() {
                        return HandlerResult::PyronovaResponse(extract_response_data(
                            py,
                            bound.clone(),
                        ));
                    }
                }
                Err(e) => {
                    return HandlerResult::PyronovaResponse(Err(format!(
                        "before_request hook error: {e}"
                    )))
                }
            }
        }

        // Main handler
        let handler_result = match handler.call1(py, (sky_req.clone(),)) {
            Ok(obj) => {
                // If handler returned a coroutine (async def), run it via asyncio
                let obj = match resolve_coroutine(py, obj) {
                    Ok(o) => o,
                    Err(e) => return HandlerResult::PyronovaResponse(Err(e)),
                };

                // Check if handler returned a PyronovaStream (SSE)
                // Use is_instance_of — single C pointer compare, no string alloc.
                let bound = obj.bind(py);
                if bound.is_instance_of::<PyronovaStream>() {
                    let stream_ref = match bound.cast::<PyronovaStream>() {
                        Ok(s) => s.get(),
                        Err(e) => return HandlerResult::PyronovaResponse(Err(e.to_string())),
                    };
                    let rx = match stream_ref.take_rx() {
                        Some(r) => r,
                        None => {
                            return HandlerResult::PyronovaResponse(Err(
                                "PyronovaStream already consumed".to_string(),
                            ))
                        }
                    };
                    let content_type = stream_ref.content_type.clone();
                    let status = stream_ref.status_code;
                    let hdrs = stream_ref.headers.clone();

                    return HandlerResult::PyronovaStream(StreamInfo {
                        rx,
                        content_type,
                        status,
                        headers: hdrs,
                    });
                }

                let resp = (|| -> Result<ResponseData, String> {
                    let mut resp_data = extract_response_data(py, obj.bind(py).clone())?;

                    // after_request hooks
                    for hook in after_hooks {
                        let body_py: Py<PyAny> = match std::str::from_utf8(&resp_data.body) {
                            Ok(s) => PyString::new(py, s).into_any().unbind(),
                            Err(_) => pyo3::types::PyBytes::new(py, &resp_data.body)
                                .into_any()
                                .unbind(),
                        };
                        let current_resp = Py::new(
                            py,
                            PyronovaResponse {
                                body: body_py,
                                status_code: resp_data.status,
                                content_type: Some(resp_data.content_type.clone()),
                                headers: resp_data.headers.clone(),
                            },
                        )
                        .map_err(|e| format!("failed to create PyronovaResponse: {e}"))?;
                        match hook.call1(py, (sky_req.clone(), current_resp)) {
                            Ok(result) => {
                                // Resolve awaitables from async after_request
                                // hooks — same reasoning as before_hooks.
                                let result = resolve_coroutine(py, result)
                                    .map_err(|e| format!("after_request hook error: {e}"))?;
                                let bound = result.bind(py);
                                if !bound.is_none() {
                                    resp_data = extract_response_data(py, bound.clone())?;
                                }
                            }
                            Err(e) => return Err(format!("after_request hook error: {e}")),
                        }
                    }

                    Ok(resp_data)
                })();
                HandlerResult::PyronovaResponse(resp)
            }
            Err(e) => {
                // Log the full PyErr (includes traceback via PyErr_Print
                // routed through the Python logging bridge) server-side.
                // Previously `{e}` forwarded a one-line repr back to the
                // client with zero operator-visible traceback AND leaked
                // internal paths to the caller. Keep the client response
                // generic; the real diagnostic lives in pyronova::server logs.
                e.display(py);
                tracing::error!(
                    target: "pyronova::server",
                    error = %e,
                    "handler raised an exception",
                );
                HandlerResult::PyronovaResponse(Err("handler error".to_string()))
            }
        };

        // Record GIL hold time before releasing GIL
        crate::monitor::GIL_HOLD_MAX_US.fetch_max(hold_start.elapsed().as_micros() as u64, Relaxed);

        handler_result
    })
}

/// Build a streaming SSE response from a channel receiver.
fn build_stream_response(info: StreamInfo) -> Response<BoxBody> {
    use tokio_stream::StreamExt;

    let stream =
        tokio_stream::wrappers::ReceiverStream::new(info.rx).map(|result| result.map(Frame::data));

    let body = StreamBody::new(stream);
    let boxed: BoxBody = BoxBody::new(body.map_err(|_| unreachable!()));

    let status = StatusCode::from_u16(info.status).unwrap_or(StatusCode::OK);
    let mut builder = Response::builder()
        .status(status)
        .header("content-type", &info.content_type)
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .header("server", crate::response::SERVER_HEADER);
    for (k, v) in &info.headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    builder
        .body(boxed)
        .unwrap_or_else(|_| Response::new(BoxBody::default()))
}

// ---------------------------------------------------------------------------
// Sub-interpreter mode handler (channel-based)
// ---------------------------------------------------------------------------

pub(crate) async fn handle_request_subinterp(
    req: Request<Incoming>,
    pool: SharedPool,
    routes: FrozenRoutes,
    client_ip_addr: std::net::IpAddr,
) -> Result<Response<BoxBody>, hyper::Error> {
    // gRPC short-circuit — see handle_request above.
    if crate::grpc::is_grpc_request(&req) {
        return crate::grpc::handle_grpc(req).await;
    }
    crate::monitor::TOTAL_REQUESTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let start = std::time::Instant::now();
    let method: Arc<str> = Arc::from(req.method().as_str());
    let uri = req.uri().clone();
    let path: Arc<str> = Arc::from(uri.path());
    let query = uri.query().unwrap_or("").to_string();

    // Fast-path: pre-built response registered via app.add_fast_response().
    // Same short-circuit as handle_request, same is_empty() guard to keep
    // apps without fast routes from paying for the String allocations.
    if !routes.fast_responses.is_empty() {
        if let Some(fr) = routes
            .fast_responses
            .get(&(method.to_string(), path.to_string()))
        {
            return Ok(full_body(build_fast_response(
                fr,
                routes.cors_config.as_ref(),
            )));
        }
    }

    // Defer header extraction — only convert if needed (sub-interp path).
    let raw_headers = req.headers().clone();
    let accept_encoding = raw_headers
        .get(hyper::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // Take ownership of the body before routing so we can decide whether
    // to collect (buffered path) or hand it to a feeder task (streaming
    // path). Mirrors the restructure done in handle_request().
    let body_obj = req.into_body();

    let lookup = pool.lookup(&method, &path);
    let has_fallback = routes.fallback_handler.is_some();

    if lookup.is_none() && (method.as_ref() == "GET" || method.as_ref() == "HEAD") {
        if let Some(resp) = try_static_file(&path, &pool.static_dirs).await {
            // Same CORS fix as handle_request — don't short-circuit
            // past apply_cors on static-file hits.
            let mut r = full_body(resp);
            apply_cors(&mut r, pool.cors_config.as_ref());
            return Ok(r);
        }
    }

    let (handler_idx, params) = match lookup {
        Some(v) => v,
        None if has_fallback => (usize::MAX, Vec::new()),
        None => {
            let mut r = full_body(not_found_response());
            apply_cors(&mut r, pool.cors_config.as_ref());
            return Ok(r);
        }
    };

    // ── Hybrid dispatch: GIL routes use main interpreter ──
    let is_gil_route =
        handler_idx == usize::MAX || pool.requires_gil.get(handler_idx).copied().unwrap_or(false);

    // Admission gate (sub-interp, large-body path only): take a permit
    // BEFORE a potentially-large body collect so that N concurrent
    // uploads can't pile N × max_body_size into RAM. The gate is
    // deliberately SKIPPED for small/no-body requests — HTTP/2
    // multiplexes hundreds of streams per connection and gcannon's
    // baseline-h2 profile easily puts 25k+ concurrent small requests
    // in flight at once. A blanket permit budget sized for "the
    // queue" (n × 128) would reject 99% of them and destroy h2
    // throughput; a budget sized for "the worst-case RAM cost of a
    // body flood" would be too large to protect against it. The
    // split: small bodies (<= ADMISSION_SKIP_BYTES) pass through,
    // large bodies (> threshold) require a permit.
    const ADMISSION_SKIP_BYTES: u64 = 64 * 1024; // 64 KiB
    let content_length = raw_headers
        .get(hyper::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let _submit_permit = if !is_gil_route && content_length > ADMISSION_SKIP_BYTES {
        match pool.submit_semaphore.clone().try_acquire_owned() {
            Ok(p) => Some(p),
            Err(_) => {
                crate::monitor::DROPPED_REQUESTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let mut r = full_body(overloaded_response("server overloaded"));
                apply_cors(&mut r, pool.cors_config.as_ref());
                return Ok(r);
            }
        }
    } else {
        None
    };

    // Streaming is only honored on GIL routes (v1). A sub-interp route
    // with stream=True falls through to the buffered path — but that's
    // impossible by construction because add_route rejects non-GIL
    // streaming at registration time.
    let is_stream_route = is_gil_route
        && handler_idx != usize::MAX
        && routes.is_stream.get(handler_idx).copied().unwrap_or(false);

    // Invariant guard: registering a stream=True route requires gil=True
    // (`add_route` enforces this). If somehow that constraint is bypassed
    // — a future refactor, a router hack — we'd spawn the feeder task and
    // then send the resulting empty `body_bytes` to a sub-interpreter
    // worker, silently discarding the client's upload. Fail closed with
    // a 500 instead of the black hole.
    let is_stream_on_subinterp = handler_idx != usize::MAX
        && !is_gil_route
        && routes.is_stream.get(handler_idx).copied().unwrap_or(false);
    if is_stream_on_subinterp {
        let mut r = full_body(error_response(
            "stream=True routes must be registered with gil=True (framework invariant violated)",
        ));
        apply_cors(&mut r, pool.cors_config.as_ref());
        return Ok(r);
    }

    // Decide body handling: stream-capable GIL routes bypass the collect
    // (proper streaming), everyone else collects up front.
    let (body_bytes, body_stream_rx_early) = if is_stream_route {
        let (tx, rx) = tokio::sync::mpsc::channel::<crate::body_stream::ChunkMsg>(
            crate::body_stream::CHANNEL_CAPACITY,
        );
        let cap = max_body_size();
        tokio::spawn(stream_body_feeder(body_obj, tx, cap));
        (
            Bytes::new(),
            Some(Arc::new(std::sync::Mutex::new(Some(rx)))),
        )
    } else {
        use http_body_util::Limited;
        let limited = Limited::new(body_obj, max_body_size());
        match tokio::time::timeout(std::time::Duration::from_secs(30), limited.collect()).await {
            Ok(Ok(c)) => (c.to_bytes(), None),
            Ok(Err(_)) => {
                let mut r = full_body(payload_too_large_response());
                apply_cors(&mut r, pool.cors_config.as_ref());
                return Ok(r);
            }
            Err(_) => {
                let mut r = full_body(crate::response::gateway_timeout_response());
                apply_cors(&mut r, pool.cors_config.as_ref());
                return Ok(r);
            }
        }
    };

    if is_gil_route {
        let method_log = Arc::clone(&method);
        let path_log = Arc::clone(&path);
        let body_stream_rx = if let Some(rx) = body_stream_rx_early.clone() {
            rx
        } else if is_stream_route {
            // Defensive dead-branch: the `is_stream_route` arm above
            // always populates body_stream_rx_early, so control never
            // reaches here. Keep it type-correct and `try_send` (non-
            // awaiting) so a future refactor doesn't accidentally
            // reintroduce a pre-awaited send on a channel nobody reads.
            let (tx, rx) = tokio::sync::mpsc::channel::<crate::body_stream::ChunkMsg>(
                crate::body_stream::CHANNEL_CAPACITY,
            );
            if !body_bytes.is_empty() {
                let _ = tx.try_send(crate::body_stream::ChunkMsg::Data(body_bytes.clone()));
            }
            let _ = tx.try_send(crate::body_stream::ChunkMsg::Eof);
            Arc::new(std::sync::Mutex::new(Some(rx)))
        } else {
            Arc::new(std::sync::Mutex::new(None))
        };
        // For stream routes, the body is served through the stream —
        // keep body_bytes empty so handler code that reads `.body`
        // doesn't double-consume.
        let body_bytes_for_req = if is_stream_route {
            Bytes::new()
        } else {
            body_bytes
        };
        let sky_req = PyronovaRequest {
            method,
            path,
            params,
            query,
            headers_source: crate::types::LazyHeaders::Raw(raw_headers),
            headers_cache: std::sync::OnceLock::new(),
            client_ip_addr,
            body_bytes: body_bytes_for_req,
            body_stream_rx,
            query_cache: std::sync::OnceLock::new(),
        };

        let routes_ref = Arc::clone(&routes);
        let handler_result = tokio::task::spawn_blocking(move || {
            call_handler_with_hooks(routes_ref, handler_idx, sky_req)
        })
        .await
        .unwrap_or_else(|_| {
            HandlerResult::PyronovaResponse(Err("handler thread panicked".to_string()))
        });

        let mut resp = match handler_result {
            HandlerResult::PyronovaResponse(mut result) => {
                if let Ok(data) = result.as_mut() {
                    crate::compression::maybe_compress(data, &accept_encoding);
                }
                full_body(build_response(result)?)
            }
            HandlerResult::PyronovaStream(info) => build_stream_response(info),
        };
        apply_cors(&mut resp, routes.cors_config.as_ref());
        let latency_us = start.elapsed().as_micros() as u64;
        let status = resp.status().as_u16();
        if routes.request_logging {
            tracing::info!(
                target: "pyronova::access",
                method = %method_log,
                path = %path_log,
                status,
                latency_us,
                mode = "gil",
                "PyronovaRequest handled"
            );
        }
        return Ok(resp);
    }

    // ── Default: sub-interpreter (fast path) ──
    // Sub-interp FFI bridge needs pre-converted headers.
    let headers = extract_headers(&raw_headers);
    let method_log = Arc::clone(&method);
    let path_log = Arc::clone(&path);
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();

    interp::WorkRequest::inc_created();
    if let Err(e) = pool.submit(interp::WorkRequest {
        handler_idx,
        method: method.to_string(),
        path: path.to_string(),
        params,
        query,
        body: body_bytes,
        headers,
        client_ip: client_ip_addr.to_string(),
        response_tx,
    }) {
        crate::monitor::DROPPED_REQUESTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut r = full_body(overloaded_response(&e));
        apply_cors(&mut r, pool.cors_config.as_ref());
        return Ok(r);
    }

    let result = match tokio::time::timeout(std::time::Duration::from_secs(30), response_rx).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => Err("worker thread dropped response".to_string()),
        Err(_) => {
            crate::monitor::DROPPED_REQUESTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut r = full_body(gateway_timeout_response());
            apply_cors(&mut r, pool.cors_config.as_ref());
            return Ok(r);
        }
    };

    let mut http_resp = match result {
        Ok(mut resp) => {
            let ct_owned: String = resp.content_type.clone().unwrap_or_else(|| {
                if resp.is_json || resp.body.starts_with(b"{") || resp.body.starts_with(b"[") {
                    "application/json".to_string()
                } else {
                    "text/plain; charset=utf-8".to_string()
                }
            });
            crate::compression::maybe_compress_subinterp(
                &mut resp.body,
                &ct_owned,
                &mut resp.headers,
                &accept_encoding,
            );
            let status = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::OK);
            let mut builder = Response::builder()
                .status(status)
                .header("content-type", &ct_owned)
                .header("server", crate::response::SERVER_HEADER);
            for (k, v) in &resp.headers {
                builder = builder.header(k.as_str(), v.as_str());
            }
            match builder.body(Full::new(Bytes::from(resp.body))) {
                Ok(r) => full_body(r),
                Err(_) => full_body(error_response("invalid response headers")),
            }
        }
        Err(e) => full_body(error_response(&e)),
    };
    // Apply CORS uniformly to both Ok and Err paths. Previously the Err
    // path returned a bare 500 with no CORS headers, so a browser would
    // surface the real error as an opaque CORS failure — a classic
    // debugging trap where the server error is invisible client-side.
    apply_cors(&mut http_resp, pool.cors_config.as_ref());
    let latency_us = start.elapsed().as_micros() as u64;
    let status = http_resp.status().as_u16();
    if routes.request_logging {
        tracing::info!(
            target: "pyronova::access",
            method = %method_log,
            path = %path_log,
            status,
            latency_us,
            mode = "subinterp",
            "PyronovaRequest handled"
        );
    }
    Ok(http_resp)
}
