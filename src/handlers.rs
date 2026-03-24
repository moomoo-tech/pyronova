use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use pyo3::prelude::*;
use pyo3::types::PyString;

use crate::interp;
use crate::response::{build_response, error_response, extract_response_data, not_found_response};
use crate::router::SharedRoutes;
use crate::static_fs::try_static_file;
use crate::types::{extract_headers, ResponseData, SkyRequest, SkyResponse};

type SharedPool = Arc<interp::InterpreterPool>;

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

    use http_body_util::BodyExt;
    let body_bytes = req
        .into_body()
        .collect()
        .await
        .map(|c| c.to_bytes().to_vec())
        .unwrap_or_default();

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

    let result: Result<ResponseData, String> = Python::attach(|py| {
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
                let mut resp_data = extract_response_data(py, obj.bind(py).clone())?;

                // after_request hooks
                for hook in &after_hooks {
                    // Preserve binary data: use PyBytes for non-UTF8
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
    });

    build_response(result)
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

    use http_body_util::BodyExt;
    let body_bytes = req
        .into_body()
        .collect()
        .await
        .map(|c| c.to_bytes().to_vec())
        .unwrap_or_default();

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
        // Route to main interpreter (full C extension support)
        let sky_req = SkyRequest {
            method,
            path,
            params,
            query,
            headers,
            body_bytes,
        };

        let result: Result<ResponseData, String> = Python::attach(|py| {
            let table = routes.read();
            let handler = table.handlers[handler_idx].clone_ref(py);
            drop(table);

            match handler.call1(py, (sky_req,)) {
                Ok(obj) => extract_response_data(py, obj.bind(py).clone()),
                Err(e) => Err(format!("handler error: {e}")),
            }
        });

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
        return Ok(error_response(&e));
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
