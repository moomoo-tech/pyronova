use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use pyo3::prelude::*;
use pyo3::types::PyString;

use crate::interp;
use crate::response::{
    build_response, error_response, extract_response_data, not_found_response,
    overloaded_response, payload_too_large_response,
};
use crate::router::SharedRoutes;
use crate::static_fs::try_static_file;
use crate::types::{extract_headers, ResponseData, SkyRequest, SkyResponse};

type SharedPool = Arc<interp::InterpreterPool>;

/// Max request body size (10 MB)
const MAX_BODY_SIZE: usize = 10 * 1024 * 1024;

/// If `obj` is a coroutine (from `async def`), execute it via a thread-local
/// asyncio event loop. Otherwise return it unchanged.
fn resolve_coroutine(py: Python<'_>, obj: Py<PyAny>) -> Result<Py<PyAny>, String> {
    let bound = obj.bind(py);
    let is_coro = unsafe { pyo3::ffi::PyCoro_CheckExact(bound.as_ptr()) == 1 };
    if !is_coro {
        return Ok(obj);
    }
    // Use thread-local persistent event loop (avoids asyncio.run() overhead
    // of creating + destroying a loop per request)
    let asyncio = py.import("asyncio").map_err(|e| format!("import asyncio: {e}"))?;

    // Use asyncio.run() — creates a temporary event loop per call.
    // Each spawn_blocking thread can run asyncio concurrently since
    // asyncio releases GIL during I/O waits.
    let result = asyncio
        .call_method1("run", (bound,))
        .map_err(|e| format!("asyncio.run error: {e}"))?;

    Ok(result.unbind())
}

// ---------------------------------------------------------------------------
// GIL mode handler
// ---------------------------------------------------------------------------

pub(crate) async fn handle_request(
    req: Request<Incoming>,
    routes: SharedRoutes,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let method = req.method().to_string();
    let uri = req.uri().clone();
    let path = uri.path().to_string();
    let query = uri.query().unwrap_or("").to_string();
    let headers = extract_headers(req.headers());

    use http_body_util::{BodyExt, Limited};
    let limited = Limited::new(req.into_body(), MAX_BODY_SIZE);
    let body_bytes = match limited.collect().await {
        Ok(c) => c.to_bytes().to_vec(),
        Err(_) => return Ok(payload_too_large_response()),
    };

    let (lookup, static_dirs, has_fallback) = {
        let table = routes.read();
        (
            table.lookup(&method, &path),
            table.static_dirs.clone(),
            table.fallback_handler.is_some(),
        )
    };

    if lookup.is_none() {
        if let Some(resp) = try_static_file(&path, &static_dirs).await {
            return Ok(resp);
        }
    }

    let (handler_idx, params) = match lookup {
        Some(v) => v,
        None if has_fallback => (usize::MAX, HashMap::new()),
        None => return Ok(not_found_response()),
    };

    let sky_req = SkyRequest {
        method,
        path,
        params,
        query,
        headers,
        body_bytes,
    };

    // spawn_blocking: prevent GIL acquisition from starving Tokio workers
    let result = tokio::task::spawn_blocking(move || {
        call_handler_with_hooks(routes, handler_idx, sky_req)
    })
    .await
    .unwrap_or_else(|_| Err("handler thread panicked".to_string()));

    build_response(result)
}

// ---------------------------------------------------------------------------
// Shared: call handler with full middleware chain (runs in blocking thread)
// ---------------------------------------------------------------------------

fn call_handler_with_hooks(
    routes: SharedRoutes,
    handler_idx: usize,
    sky_req: SkyRequest,
) -> Result<ResponseData, String> {
    Python::attach(|py| {
        let table = routes.read();
        let before_hooks: Vec<Py<PyAny>> =
            table.before_hooks.iter().map(|h| h.clone_ref(py)).collect();
        let after_hooks: Vec<Py<PyAny>> =
            table.after_hooks.iter().map(|h| h.clone_ref(py)).collect();

        let handler = if handler_idx == usize::MAX {
            table.fallback_handler.as_ref().unwrap().clone_ref(py)
        } else {
            table.handlers[handler_idx].clone_ref(py)
        };
        drop(table);

        // before_request hooks
        for hook in &before_hooks {
            match hook.call1(py, (sky_req.clone(),)) {
                Ok(result) => {
                    let bound = result.bind(py);
                    if !bound.is_none() {
                        return extract_response_data(py, bound.clone());
                    }
                }
                Err(e) => return Err(format!("before_request hook error: {e}")),
            }
        }

        // Main handler
        match handler.call1(py, (sky_req.clone(),)) {
            Ok(obj) => {
                // If handler returned a coroutine (async def), run it via asyncio
                let obj = resolve_coroutine(py, obj)?;
                let mut resp_data = extract_response_data(py, obj.bind(py).clone())?;

                // after_request hooks
                for hook in &after_hooks {
                    let body_py: Py<PyAny> = match std::str::from_utf8(&resp_data.body) {
                        Ok(s) => PyString::new(py, s).into_any().unbind(),
                        Err(_) => pyo3::types::PyBytes::new(py, &resp_data.body)
                            .into_any()
                            .unbind(),
                    };
                    let current_resp = Py::new(
                        py,
                        SkyResponse {
                            body: body_py,
                            status_code: resp_data.status,
                            content_type: Some(resp_data.content_type.clone()),
                            headers: resp_data.headers.clone(),
                        },
                    )
                    .map_err(|e| format!("failed to create SkyResponse: {e}"))?;
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
            }
            Err(e) => Err(format!("handler error: {e}")),
        }
    })
}

// ---------------------------------------------------------------------------
// Sub-interpreter mode handler (channel-based)
// ---------------------------------------------------------------------------

pub(crate) async fn handle_request_subinterp(
    req: Request<Incoming>,
    pool: SharedPool,
    routes: SharedRoutes,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let method = req.method().to_string();
    let uri = req.uri().clone();
    let path = uri.path().to_string();
    let query = uri.query().unwrap_or("").to_string();
    let headers = extract_headers(req.headers());

    use http_body_util::{BodyExt, Limited};
    let limited = Limited::new(req.into_body(), MAX_BODY_SIZE);
    let body_bytes = match limited.collect().await {
        Ok(c) => c.to_bytes().to_vec(),
        Err(_) => return Ok(payload_too_large_response()),
    };

    let lookup = pool.lookup(&method, &path);

    if lookup.is_none() {
        if let Some(resp) = try_static_file(&path, &pool.static_dirs).await {
            return Ok(resp);
        }
    }

    let (handler_idx, params) = match lookup {
        Some(v) => v,
        None => return Ok(not_found_response()),
    };

    // ── Hybrid dispatch: GIL routes use main interpreter ──
    let is_gil_route = pool.requires_gil.get(handler_idx).copied().unwrap_or(false);

    if is_gil_route {
        // GIL route: spawn_blocking to avoid starving Tokio workers
        let sky_req = SkyRequest {
            method,
            path,
            params,
            query,
            headers,
            body_bytes,
        };

        let result = tokio::task::spawn_blocking(move || {
            call_handler_with_hooks(routes, handler_idx, sky_req)
        })
        .await
        .unwrap_or_else(|_| Err("handler thread panicked".to_string()));

        return build_response(result);
    }

    // ── Default: sub-interpreter (fast path) ──
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();

    if let Err(e) = pool.submit(interp::WorkRequest {
        handler_idx,
        method,
        path,
        params,
        query,
        body: body_bytes,
        headers,
        response_tx,
    }) {
        return Ok(overloaded_response(&e));
    }

    let result = match response_rx.await {
        Ok(r) => r,
        Err(_) => Err("worker thread dropped response".to_string()),
    };

    match result {
        Ok(resp) => {
            let ct = resp.content_type.unwrap_or_else(|| {
                if resp.is_json || resp.body.starts_with('{') || resp.body.starts_with('[') {
                    "application/json".to_string()
                } else {
                    "text/plain; charset=utf-8".to_string()
                }
            });
            let status = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::OK);
            let mut builder = Response::builder()
                .status(status)
                .header("content-type", &ct)
                .header("server", "Pyre/0.3.1-subinterp");
            for (k, v) in &resp.headers {
                builder = builder.header(k.as_str(), v.as_str());
            }
            Ok(builder.body(Full::new(Bytes::from(resp.body))).unwrap())
        }
        Err(e) => Ok(error_response(&e)),
    }
}
