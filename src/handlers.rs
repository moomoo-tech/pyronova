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
use crate::stream::PyreStream;
use crate::types::{extract_headers, PyreRequest, PyreResponse, ResponseData};

type SharedPool = Arc<interp::InterpreterPool>;

/// Default max request body size (10 MB). Configurable via `app.max_body_size`.
const DEFAULT_MAX_BODY_SIZE: usize = 10 * 1024 * 1024;

/// Global max body size — set once at startup, read on every request (lock-free).
static MAX_BODY_SIZE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(DEFAULT_MAX_BODY_SIZE);

pub(crate) fn set_max_body_size(size: usize) {
    MAX_BODY_SIZE.store(size, std::sync::atomic::Ordering::Relaxed);
}

fn max_body_size() -> usize {
    MAX_BODY_SIZE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Result from handler: either a normal response or a stream.
enum HandlerResult {
    Response(Result<ResponseData, String>),
    Stream(StreamInfo),
}

struct StreamInfo {
    rx: tokio::sync::mpsc::UnboundedReceiver<Result<Bytes, std::convert::Infallible>>,
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
    struct LoopGuard(Option<Py<PyAny>>);
    impl Drop for LoopGuard {
        fn drop(&mut self) {
            if let Some(loop_obj) = self.0.take() {
                Python::attach(|py| {
                    let _ = loop_obj.call_method0(py, "close");
                });
            }
        }
    }

    thread_local! {
        static LOOP: RefCell<LoopGuard> = const { RefCell::new(LoopGuard(None)) };
    }

    let bound = obj.bind(py);
    let is_coro = unsafe { pyo3::ffi::PyCoro_CheckExact(bound.as_ptr()) == 1 };
    if !is_coro {
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

pub(crate) fn full_body(resp: Response<Full<Bytes>>) -> Response<BoxBody> {
    resp.map(|b| b.map_err(|_| unreachable!()).boxed())
}

pub(crate) async fn handle_request(
    req: Request<Incoming>,
    routes: FrozenRoutes,
    client_ip_addr: std::net::IpAddr,
) -> Result<Response<BoxBody>, hyper::Error> {
    crate::monitor::TOTAL_REQUESTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let start = std::time::Instant::now();
    let method: Arc<str> = Arc::from(req.method().as_str());
    let uri = req.uri().clone();
    let path: Arc<str> = Arc::from(uri.path());
    let query = uri.query().unwrap_or("").to_string();
    // Lazy headers: store raw HeaderMap, convert only if Python accesses req.headers.
    let raw_headers = req.headers().clone();

    use http_body_util::Limited;
    let limited = Limited::new(req.into_body(), max_body_size());
    let body_bytes = match limited.collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return Ok(full_body(payload_too_large_response())),
    };

    let lookup = routes.lookup(&method, &path);
    let has_fallback = routes.fallback_handler.is_some();

    if lookup.is_none() {
        if let Some(resp) = try_static_file(&path, &routes.static_dirs).await {
            return Ok(full_body(resp));
        }
    }

    let (handler_idx, params) = match lookup {
        Some(v) => v,
        None if has_fallback => (usize::MAX, Vec::new()),
        None => return Ok(full_body(not_found_response())),
    };

    let method_log = Arc::clone(&method);
    let path_log = Arc::clone(&path);
    let sky_req = PyreRequest {
        method,
        path,
        params,
        query,
        headers_source: crate::types::LazyHeaders::Raw(raw_headers),
        headers_cache: std::sync::OnceLock::new(),
        client_ip_addr,
        body_bytes,
    };

    // spawn_blocking: prevent GIL acquisition from starving Tokio workers
    let routes_ref = Arc::clone(&routes);
    let handler_result =
        tokio::task::spawn_blocking(move || call_handler_with_hooks(routes_ref, handler_idx, sky_req))
            .await
            .unwrap_or_else(|_| {
                HandlerResult::Response(Err("handler thread panicked".to_string()))
            });

    let mut resp = match handler_result {
        HandlerResult::Response(result) => full_body(build_response(result)?),
        HandlerResult::Stream(info) => build_stream_response(info),
    };

    // Apply CORS headers if configured (matches sub-interpreter behavior)
    if let Some(origin) = routes.cors_origin.as_ref() {
        let headers = resp.headers_mut();
        headers.insert("access-control-allow-origin", origin.parse().unwrap());
        headers.insert(
            "access-control-allow-methods",
            "GET, POST, PUT, DELETE, PATCH, OPTIONS".parse().unwrap(),
        );
        headers.insert("access-control-allow-headers", "*".parse().unwrap());
    }

    let latency_us = start.elapsed().as_micros() as u64;
    let status = resp.status().as_u16();
    if routes.request_logging {
        tracing::info!(
            target: "pyre::access",
            method = %method_log,
            path = %path_log,
            status,
            latency_us,
            mode = "gil",
            "Request handled"
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
    sky_req: PyreRequest,
) -> HandlerResult {
    use std::sync::atomic::Ordering::Relaxed;

    // Track GIL queue: +1 before acquiring, -1 after acquiring
    crate::monitor::GIL_QUEUE_LENGTH.fetch_add(1, Relaxed);

    Python::attach(|py| {
        crate::monitor::GIL_QUEUE_LENGTH.fetch_sub(1, Relaxed);
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
        for hook in before_hooks {
            match hook.call1(py, (sky_req.clone(),)) {
                Ok(result) => {
                    let bound = result.bind(py);
                    if !bound.is_none() {
                        return HandlerResult::Response(extract_response_data(py, bound.clone()));
                    }
                }
                Err(e) => {
                    return HandlerResult::Response(Err(format!("before_request hook error: {e}")))
                }
            }
        }

        // Main handler
        let handler_result = match handler.call1(py, (sky_req.clone(),)) {
            Ok(obj) => {
                // If handler returned a coroutine (async def), run it via asyncio
                let obj = match resolve_coroutine(py, obj) {
                    Ok(o) => o,
                    Err(e) => return HandlerResult::Response(Err(e)),
                };

                // Check if handler returned a PyreStream (SSE)
                // Use is_instance_of — single C pointer compare, no string alloc.
                let bound = obj.bind(py);
                if bound.is_instance_of::<PyreStream>() {
                    let stream_ref = match bound.cast::<PyreStream>() {
                        Ok(s) => s.get(),
                        Err(e) => return HandlerResult::Response(Err(e.to_string())),
                    };
                    let rx = match stream_ref.take_rx() {
                        Some(r) => r,
                        None => {
                            return HandlerResult::Response(Err(
                                "PyreStream already consumed".to_string()
                            ))
                        }
                    };
                    let content_type = stream_ref.content_type.clone();
                    let status = stream_ref.status_code;
                    let hdrs = stream_ref.headers.clone();

                    return HandlerResult::Stream(StreamInfo {
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
                            PyreResponse {
                                body: body_py,
                                status_code: resp_data.status,
                                content_type: Some(resp_data.content_type.clone()),
                                headers: resp_data.headers.clone(),
                            },
                        )
                        .map_err(|e| format!("failed to create PyreResponse: {e}"))?;
                        match hook.call1(py, (sky_req.clone(), current_resp)) {
                            Ok(result) => {
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
                HandlerResult::Response(resp)
            }
            Err(e) => HandlerResult::Response(Err(format!("handler error: {e}"))),
        };

        // Record GIL hold time before releasing GIL
        crate::monitor::GIL_HOLD_MAX_US.fetch_max(hold_start.elapsed().as_micros() as u64, Relaxed);

        handler_result
    })
}

/// Build a streaming SSE response from a channel receiver.
fn build_stream_response(info: StreamInfo) -> Response<BoxBody> {
    use tokio_stream::StreamExt;

    let stream = tokio_stream::wrappers::UnboundedReceiverStream::new(info.rx)
        .map(|result| result.map(Frame::data));

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
    builder.body(boxed).unwrap()
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
    crate::monitor::TOTAL_REQUESTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let start = std::time::Instant::now();
    let method: Arc<str> = Arc::from(req.method().as_str());
    let uri = req.uri().clone();
    let path: Arc<str> = Arc::from(uri.path());
    let query = uri.query().unwrap_or("").to_string();
    // Defer header extraction — only convert if needed (sub-interp path).
    let raw_headers = req.headers().clone();

    use http_body_util::Limited;
    let limited = Limited::new(req.into_body(), max_body_size());
    let body_bytes = match limited.collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return Ok(full_body(payload_too_large_response())),
    };

    let lookup = pool.lookup(&method, &path);
    let has_fallback = routes.fallback_handler.is_some();

    if lookup.is_none() {
        if let Some(resp) = try_static_file(&path, &pool.static_dirs).await {
            return Ok(full_body(resp));
        }
    }

    let (handler_idx, params) = match lookup {
        Some(v) => v,
        None if has_fallback => (usize::MAX, Vec::new()),
        None => return Ok(full_body(not_found_response())),
    };

    // ── Hybrid dispatch: GIL routes use main interpreter ──
    let is_gil_route =
        handler_idx == usize::MAX || pool.requires_gil.get(handler_idx).copied().unwrap_or(false);

    if is_gil_route {
        let method_log = Arc::clone(&method);
        let path_log = Arc::clone(&path);
        let sky_req = PyreRequest {
            method,
            path,
            params,
            query,
            headers_source: crate::types::LazyHeaders::Raw(raw_headers),
            headers_cache: std::sync::OnceLock::new(),
            client_ip_addr,
            body_bytes,
        };

        let handler_result = tokio::task::spawn_blocking(move || {
            call_handler_with_hooks(routes, handler_idx, sky_req)
        })
        .await
        .unwrap_or_else(|_| HandlerResult::Response(Err("handler thread panicked".to_string())));

        let resp = match handler_result {
            HandlerResult::Response(result) => full_body(build_response(result)?),
            HandlerResult::Stream(info) => build_stream_response(info),
        };
        let latency_us = start.elapsed().as_micros() as u64;
        let status = resp.status().as_u16();
        tracing::info!(
            target: "pyre::access",
            method = %method_log,
            path = %path_log,
            status,
            latency_us,
            mode = "gil",
            "Request handled"
        );
        return Ok(resp);
    }

    // ── Default: sub-interpreter (fast path) ──
    // Sub-interp FFI bridge needs pre-converted headers.
    let headers = extract_headers(&raw_headers);
    let method_log = Arc::clone(&method);
    let path_log = Arc::clone(&path);
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();

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
        return Ok(full_body(overloaded_response(&e)));
    }

    let result = match tokio::time::timeout(std::time::Duration::from_secs(30), response_rx).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => Err("worker thread dropped response".to_string()),
        Err(_) => {
            crate::monitor::DROPPED_REQUESTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Ok(full_body(gateway_timeout_response()));
        }
    };

    let http_resp = match result {
        Ok(resp) => {
            let ct = resp.content_type.as_deref().unwrap_or_else(|| {
                if resp.is_json || resp.body.starts_with(b"{") || resp.body.starts_with(b"[") {
                    "application/json"
                } else {
                    "text/plain; charset=utf-8"
                }
            });
            let status = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::OK);
            let mut builder = Response::builder()
                .status(status)
                .header("content-type", ct)
                .header("server", crate::response::SERVER_HEADER);
            for (k, v) in &resp.headers {
                builder = builder.header(k.as_str(), v.as_str());
            }
            // Add CORS headers if enabled (per-instance config)
            if let Some(origin) = pool.cors_origin.as_ref() {
                builder = builder.header("access-control-allow-origin", origin.as_str());
                builder = builder.header(
                    "access-control-allow-methods",
                    "GET, POST, PUT, DELETE, PATCH, OPTIONS",
                );
                builder = builder.header("access-control-allow-headers", "*");
            }
            full_body(builder.body(Full::new(Bytes::from(resp.body))).unwrap())
        }
        Err(e) => full_body(error_response(&e)),
    };
    let latency_us = start.elapsed().as_micros() as u64;
    let status = http_resp.status().as_u16();
    if routes.request_logging {
        tracing::info!(
            target: "pyre::access",
            method = %method_log,
            path = %path_log,
            status,
            latency_us,
            mode = "subinterp",
            "Request handled"
        );
    }
    Ok(http_resp)
}
