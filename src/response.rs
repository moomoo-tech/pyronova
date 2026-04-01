use std::collections::HashMap;

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Response, StatusCode};

pub(crate) const SERVER_HEADER: &str = concat!("Pyre/", env!("CARGO_PKG_VERSION"));
use pyo3::prelude::*;
use pyo3::types::{PyAnyMethods, PyDict, PyList, PyString};

use crate::json::py_to_json_value;
use crate::types::{PyreResponse, ResponseData};

// ---------------------------------------------------------------------------
// Extract handler return value → ResponseData
// ---------------------------------------------------------------------------

pub(crate) fn extract_response_data(
    py: Python<'_>,
    obj: Bound<'_, pyo3::PyAny>,
) -> Result<ResponseData, String> {
    // PyreResponse
    if let Ok(resp) = obj.cast::<PyreResponse>() {
        let resp = resp.get();
        let body_bound = resp.body.bind(py);

        let (body_bytes, auto_ct) = if let Ok(s) = body_bound.cast::<PyString>() {
            let st = s.to_string();
            let ct = if st.starts_with('{') || st.starts_with('[') {
                "application/json"
            } else {
                "text/plain; charset=utf-8"
            };
            (Bytes::from(st), ct)
        } else if body_bound.cast::<PyDict>().is_ok() || body_bound.cast::<PyList>().is_ok() {
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
    if let Ok(s) = obj.cast::<PyString>() {
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
    if obj.cast::<PyDict>().is_ok() {
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
    if obj.cast::<PyList>().is_ok() {
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
                .header("server", SERVER_HEADER);
            for (k, v) in &data.headers {
                builder = builder.header(k.as_str(), v.as_str());
            }
            Ok(builder
                .body(Full::new(data.body))
                .unwrap_or_else(|_| error_response("invalid response headers")))
        }
        Err(e) => Ok(error_response(&e)),
    }
}

pub(crate) fn error_response(msg: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .header("content-type", "application/json")
        .header("server", SERVER_HEADER)
        .body(Full::new(Bytes::from(format!(
            r#"{{"error":"{}"}}"#,
            msg.replace('"', "\\\"")
        ))))
        .unwrap()
}

#[inline]
pub(crate) fn overloaded_response(msg: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header("content-type", "application/json")
        .header("server", SERVER_HEADER)
        .header("retry-after", "1")
        .body(Full::new(Bytes::from(format!(
            r#"{{"error":"{}"}}"#,
            msg.replace('"', "\\\"")
        ))))
        .unwrap()
}

#[inline]
pub(crate) fn payload_too_large_response() -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::PAYLOAD_TOO_LARGE)
        .header("content-type", "application/json")
        .header("server", SERVER_HEADER)
        .body(Full::new(Bytes::from_static(
            b"{\"error\":\"payload too large\"}",
        )))
        .unwrap()
}

#[inline]
pub(crate) fn gateway_timeout_response() -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::GATEWAY_TIMEOUT)
        .header("content-type", "application/json")
        .header("server", SERVER_HEADER)
        .body(Full::new(Bytes::from_static(
            b"{\"error\":\"request timeout\"}",
        )))
        .unwrap()
}

#[inline]
pub(crate) fn not_found_response() -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header("content-type", "application/json")
        .header("server", SERVER_HEADER)
        .body(Full::new(Bytes::from_static(b"{\"error\":\"not found\"}")))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn body_bytes(resp: Response<Full<Bytes>>) -> Vec<u8> {
        use http_body_util::BodyExt;
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let collected = resp.into_body().collect().await.unwrap();
            collected.to_bytes().to_vec()
        })
    }

    #[test]
    fn not_found_status_and_body() {
        let resp = not_found_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(resp.headers()["content-type"], "application/json");
        assert!(resp.headers()["server"]
            .to_str()
            .unwrap()
            .starts_with("Pyre/"));
        assert_eq!(body_bytes(resp), b"{\"error\":\"not found\"}");
    }

    #[test]
    fn error_response_500() {
        let resp = error_response("something broke");
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(resp.headers()["content-type"], "application/json");
        let body = String::from_utf8(body_bytes(resp)).unwrap();
        assert!(body.contains("something broke"));
    }

    #[test]
    fn error_response_escapes_quotes() {
        let resp = error_response(r#"bad "input""#);
        let body = String::from_utf8(body_bytes(resp)).unwrap();
        assert!(body.contains(r#"bad \"input\""#));
    }

    #[test]
    fn overloaded_503_with_retry_after() {
        let resp = overloaded_response("too busy");
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(resp.headers()["retry-after"], "1");
        let body = String::from_utf8(body_bytes(resp)).unwrap();
        assert!(body.contains("too busy"));
    }

    #[test]
    fn payload_too_large_413() {
        let resp = payload_too_large_response();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = String::from_utf8(body_bytes(resp)).unwrap();
        assert!(body.contains("payload too large"));
    }

    #[test]
    fn gateway_timeout_504() {
        let resp = gateway_timeout_response();
        assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
        let body = String::from_utf8(body_bytes(resp)).unwrap();
        assert!(body.contains("request timeout"));
    }

    #[test]
    fn build_response_ok() {
        let data = ResponseData {
            body: Bytes::from("hello"),
            content_type: "text/plain".to_string(),
            status: 200,
            headers: HashMap::new(),
        };
        let resp = build_response(Ok(data)).unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["content-type"], "text/plain");
    }

    #[test]
    fn build_response_custom_status_and_headers() {
        let mut headers = HashMap::new();
        headers.insert("x-custom".to_string(), "value".to_string());
        let data = ResponseData {
            body: Bytes::from("created"),
            content_type: "application/json".to_string(),
            status: 201,
            headers,
        };
        let resp = build_response(Ok(data)).unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        assert_eq!(resp.headers()["x-custom"], "value");
    }

    #[test]
    fn build_response_error_falls_back_to_500() {
        let resp = build_response(Err("oops".to_string())).unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
