use std::collections::HashMap;

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Response, StatusCode};
use pyo3::prelude::*;
use pyo3::types::{PyAnyMethods, PyDict, PyList, PyString};

use crate::json::py_to_json_value;
use crate::types::{ResponseData, SkyResponse};

// ---------------------------------------------------------------------------
// Extract handler return value → ResponseData
// ---------------------------------------------------------------------------

pub(crate) fn extract_response_data(
    py: Python<'_>,
    obj: Bound<'_, pyo3::PyAny>,
) -> Result<ResponseData, String> {
    // Detect async def handlers — give a clear error instead of returning <coroutine>
    if obj.get_type().name().map(|n| n.to_string()).unwrap_or_default() == "coroutine" {
        return Err(
            "async handlers are not supported. Use `def handler(req)` instead of \
             `async def handler(req)`. Pyre uses real multi-threading (sub-interpreters), \
             so async is not needed for concurrency."
                .to_string(),
        );
    }

    // SkyResponse
    if let Ok(resp) = obj.downcast::<SkyResponse>() {
        let resp = resp.get();
        let body_bound = resp.body.bind(py);

        let (body_bytes, auto_ct) = if let Ok(s) = body_bound.downcast::<PyString>() {
            let st = s.to_string();
            let ct = if st.starts_with('{') || st.starts_with('[') {
                "application/json"
            } else {
                "text/plain; charset=utf-8"
            };
            (Bytes::from(st), ct)
        } else if body_bound.downcast::<PyDict>().is_ok()
            || body_bound.downcast::<PyList>().is_ok()
        {
            let val = py_to_json_value(body_bound).map_err(|e| format!("json error: {e}"))?;
            let json_bytes =
                serde_json::to_vec(&val).map_err(|e| format!("json serialize error: {e}"))?;
            (Bytes::from(json_bytes), "application/json")
        } else if let Ok(b) = body_bound.extract::<Vec<u8>>() {
            (Bytes::from(b), "application/octet-stream")
        } else {
            let st = body_bound.str().map_err(|e| e.to_string())?.to_string();
            (Bytes::from(st), "text/plain; charset=utf-8")
        };

        let content_type = resp
            .content_type
            .clone()
            .unwrap_or_else(|| auto_ct.to_string());

        return Ok(ResponseData {
            body: body_bytes,
            content_type,
            status: resp.status_code,
            headers: resp.headers.clone(),
        });
    }

    // Plain string
    if let Ok(s) = obj.downcast::<PyString>() {
        let st = s.to_string();
        let ct = if st.starts_with('{') || st.starts_with('[') {
            "application/json"
        } else {
            "text/plain; charset=utf-8"
        };
        return Ok(ResponseData {
            body: Bytes::from(st),
            content_type: ct.to_string(),
            status: 200,
            headers: HashMap::new(),
        });
    }

    // dict → JSON
    if obj.downcast::<PyDict>().is_ok() {
        let val = py_to_json_value(&obj).map_err(|e| format!("json error: {e}"))?;
        let json_bytes =
            serde_json::to_vec(&val).map_err(|e| format!("json serialize error: {e}"))?;
        return Ok(ResponseData {
            body: Bytes::from(json_bytes),
            content_type: "application/json".to_string(),
            status: 200,
            headers: HashMap::new(),
        });
    }

    // list → JSON
    if obj.downcast::<PyList>().is_ok() {
        let val = py_to_json_value(&obj).map_err(|e| format!("json error: {e}"))?;
        let json_bytes =
            serde_json::to_vec(&val).map_err(|e| format!("json serialize error: {e}"))?;
        return Ok(ResponseData {
            body: Bytes::from(json_bytes),
            content_type: "application/json".to_string(),
            status: 200,
            headers: HashMap::new(),
        });
    }

    // bytes
    if let Ok(b) = obj.extract::<Vec<u8>>() {
        return Ok(ResponseData {
            body: Bytes::from(b),
            content_type: "application/octet-stream".to_string(),
            status: 200,
            headers: HashMap::new(),
        });
    }

    // fallback: str()
    let st = obj.str().map_err(|e| e.to_string())?.to_string();
    Ok(ResponseData {
        body: Bytes::from(st),
        content_type: "text/plain; charset=utf-8".to_string(),
        status: 200,
        headers: HashMap::new(),
    })
}

// ---------------------------------------------------------------------------
// HTTP response builders
// ---------------------------------------------------------------------------

pub(crate) fn build_response(
    result: Result<ResponseData, String>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    match result {
        Ok(data) => {
            let status = StatusCode::from_u16(data.status).unwrap_or(StatusCode::OK);
            let mut builder = Response::builder()
                .status(status)
                .header("content-type", &data.content_type)
                .header("server", "Pyre/0.3.0");
            for (k, v) in &data.headers {
                builder = builder.header(k.as_str(), v.as_str());
            }
            Ok(builder.body(Full::new(data.body)).unwrap())
        }
        Err(e) => Ok(error_response(&e)),
    }
}

pub(crate) fn error_response(msg: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .header("content-type", "application/json")
        .header("server", "Pyre/0.3.0")
        .body(Full::new(Bytes::from(
            format!(r#"{{"error":"{}"}}"#, msg.replace('"', "\\\"")),
        )))
        .unwrap()
}

#[inline]
pub(crate) fn overloaded_response(msg: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header("content-type", "application/json")
        .header("server", "Pyre/0.4.0")
        .header("retry-after", "1")
        .body(Full::new(Bytes::from(
            format!(r#"{{"error":"{}"}}"#, msg.replace('"', "\\\"")),
        )))
        .unwrap()
}

#[inline]
pub(crate) fn payload_too_large_response() -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::PAYLOAD_TOO_LARGE)
        .header("content-type", "application/json")
        .header("server", "Pyre/0.3.1")
        .body(Full::new(Bytes::from_static(
            b"{\"error\":\"payload too large\"}",
        )))
        .unwrap()
}

#[inline]
pub(crate) fn not_found_response() -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header("content-type", "application/json")
        .header("server", "Pyre/0.3.0")
        .body(Full::new(Bytes::from_static(b"{\"error\":\"not found\"}")))
        .unwrap()
}
