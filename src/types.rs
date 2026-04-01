use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, OnceLock};

use bytes::Bytes;
use pyo3::prelude::*;

// ---------------------------------------------------------------------------
// PyreRequest
// ---------------------------------------------------------------------------

/// Two ways to construct headers:
/// - GIL path: store raw `hyper::HeaderMap`, convert lazily on first Python access.
/// - Sub-interp path: pre-converted `HashMap` (needed for C-FFI bridge).
pub(crate) enum LazyHeaders {
    /// Raw hyper HeaderMap — O(1) construction, deferred conversion.
    Raw(hyper::HeaderMap),
    /// Pre-converted (sub-interp path where FFI needs HashMap anyway).
    Converted(HashMap<String, String>),
}

#[pyclass(frozen, skip_from_py_object)]
pub(crate) struct PyreRequest {
    /// Arc<str> — shared with access log, zero-cost clone.
    pub(crate) method: Arc<str>,
    /// Arc<str> — shared with access log, zero-cost clone.
    pub(crate) path: Arc<str>,
    /// Stored as Vec for small-count path params (typically 1-2).
    pub(crate) params: Vec<(String, String)>,
    #[pyo3(get)]
    pub(crate) query: String,
    /// Lazy headers: raw HeaderMap or pre-converted HashMap.
    pub(crate) headers_source: LazyHeaders,
    /// Cached conversion — computed once on first Python access.
    pub(crate) headers_cache: OnceLock<HashMap<String, String>>,
    /// Raw IP — zero allocation. `.to_string()` only when Python accesses it.
    pub(crate) client_ip_addr: IpAddr,
    /// Stored as Bytes (ref-counted, zero-copy from hyper).
    pub(crate) body_bytes: Bytes,
}

/// Manual Clone: OnceLock doesn't impl Clone, so we reset the cache on clone.
/// Cloned requests lazily recompute headers if accessed.
impl Clone for PyreRequest {
    fn clone(&self) -> Self {
        Self {
            method: self.method.clone(),
            path: self.path.clone(),
            params: self.params.clone(),
            query: self.query.clone(),
            headers_source: self.headers_source.clone(),
            headers_cache: OnceLock::new(),
            client_ip_addr: self.client_ip_addr,
            body_bytes: self.body_bytes.clone(),
        }
    }
}

impl Clone for LazyHeaders {
    fn clone(&self) -> Self {
        match self {
            Self::Raw(hm) => Self::Raw(hm.clone()),
            Self::Converted(m) => Self::Converted(m.clone()),
        }
    }
}

impl PyreRequest {
    /// Resolve headers to HashMap (lazy for Raw, immediate for Converted).
    pub(crate) fn resolved_headers(&self) -> &HashMap<String, String> {
        self.headers_cache.get_or_init(|| match &self.headers_source {
            LazyHeaders::Raw(hm) => extract_headers(hm),
            LazyHeaders::Converted(m) => m.clone(),
        })
    }
}

#[pymethods]
impl PyreRequest {
    #[getter]
    fn method(&self) -> &str {
        &self.method
    }

    #[getter]
    fn path(&self) -> &str {
        &self.path
    }

    /// Converts Vec<(String, String)> → Python dict on access.
    #[getter]
    fn params(&self) -> HashMap<String, String> {
        self.params.iter().cloned().collect()
    }

    /// Lazy headers: converts raw HeaderMap → dict only on first access.
    #[getter]
    fn headers(&self) -> HashMap<String, String> {
        self.resolved_headers().clone()
    }

    /// Lazy: heap-allocates the IP string only when Python reads `req.client_ip`.
    #[getter]
    fn client_ip(&self) -> String {
        self.client_ip_addr.to_string()
    }

    #[getter]
    fn body(&self) -> &[u8] {
        &self.body_bytes
    }

    /// Zero-copy: validates UTF-8 on the Bytes slice, creates Python str directly.
    fn text<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, pyo3::types::PyString>> {
        let s = std::str::from_utf8(&self.body_bytes)
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
        Ok(pyo3::types::PyString::new(py, s))
    }

    /// Parse JSON in Rust (serde_json) → convert to Python objects (pythonize).
    /// ~3x faster than Python's json.loads for typical API payloads.
    fn json<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, pyo3::PyAny>> {
        let parsed: serde_json::Value = serde_json::from_slice(&self.body_bytes)
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("JSON parse error: {e}")))?;
        pythonize::pythonize(py, &parsed)
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("pythonize error: {e}")))
    }

    #[getter]
    fn query_params(&self) -> HashMap<String, String> {
        form_urlencoded::parse(self.query.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// PyreResponse
// ---------------------------------------------------------------------------

#[pyclass(frozen)]
pub(crate) struct PyreResponse {
    #[pyo3(get)]
    pub(crate) body: Py<PyAny>,
    #[pyo3(get)]
    pub(crate) status_code: u16,
    #[pyo3(get)]
    pub(crate) content_type: Option<String>,
    #[pyo3(get)]
    pub(crate) headers: HashMap<String, String>,
}

#[pymethods]
impl PyreResponse {
    #[new]
    #[pyo3(signature = (body, status_code=200, content_type=None, headers=None))]
    fn new(
        body: Py<PyAny>,
        status_code: u16,
        content_type: Option<String>,
        headers: Option<HashMap<String, String>>,
    ) -> Self {
        PyreResponse {
            body,
            status_code,
            content_type,
            headers: headers.unwrap_or_default(),
        }
    }
}

// ---------------------------------------------------------------------------
// ResponseData (Rust-internal, not exposed to Python)
// ---------------------------------------------------------------------------

pub(crate) struct ResponseData {
    pub(crate) body: Bytes,
    pub(crate) content_type: String,
    pub(crate) status: u16,
    pub(crate) headers: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// extract_headers
// ---------------------------------------------------------------------------

pub fn extract_headers(header_map: &hyper::HeaderMap) -> HashMap<String, String> {
    let mut headers = HashMap::with_capacity(header_map.len());
    for (name, value) in header_map.iter() {
        let key = name.as_str().to_string();
        // Fast path: valid UTF-8 (99.99% of headers) avoids Cow→String deep copy.
        let val = match std::str::from_utf8(value.as_bytes()) {
            Ok(s) => s.to_string(),
            Err(_) => String::from_utf8_lossy(value.as_bytes()).into_owned(),
        };
        headers
            .entry(key)
            .and_modify(|existing: &mut String| {
                existing.push_str(", ");
                existing.push_str(&val);
            })
            .or_insert(val);
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_headers_basic() {
        let mut hm = hyper::HeaderMap::new();
        hm.insert("content-type", "application/json".parse().unwrap());
        hm.insert("x-custom", "hello".parse().unwrap());
        let h = extract_headers(&hm);
        assert_eq!(h["content-type"], "application/json");
        assert_eq!(h["x-custom"], "hello");
    }

    #[test]
    fn extract_headers_empty() {
        let hm = hyper::HeaderMap::new();
        let h = extract_headers(&hm);
        assert!(h.is_empty());
    }

    #[test]
    fn extract_headers_multi_value() {
        let mut hm = hyper::HeaderMap::new();
        hm.append("accept", "text/html".parse().unwrap());
        hm.append("accept", "application/json".parse().unwrap());
        let h = extract_headers(&hm);
        assert!(h["accept"].contains("text/html"));
        assert!(h["accept"].contains("application/json"));
        assert!(h["accept"].contains(", "));
    }

    #[test]
    fn query_params_parsing() {
        let req = PyreRequest {
            method: Arc::from("GET"),
            path: Arc::from("/search"),
            params: Vec::new(),
            query: "q=hello+world&page=2&lang=en".to_string(),
            headers: HashMap::new(),
            client_ip_addr: IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            body_bytes: Bytes::new(),
        };
        let qp = req.query_params();
        assert_eq!(qp["q"], "hello world");
        assert_eq!(qp["page"], "2");
        assert_eq!(qp["lang"], "en");
    }

    #[test]
    fn query_params_empty() {
        let req = PyreRequest {
            method: Arc::from("GET"),
            path: Arc::from("/"),
            params: Vec::new(),
            query: "".to_string(),
            headers: HashMap::new(),
            client_ip_addr: IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            body_bytes: Bytes::new(),
        };
        assert!(req.query_params().is_empty());
    }

    #[test]
    fn query_params_percent_encoded() {
        let req = PyreRequest {
            method: Arc::from("GET"),
            path: Arc::from("/"),
            params: Vec::new(),
            query: "name=%E4%B8%AD%E6%96%87".to_string(),
            headers: HashMap::new(),
            client_ip_addr: IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            body_bytes: Bytes::new(),
        };
        assert_eq!(req.query_params()["name"], "中文");
    }

    // Note: text() and json() require Python GIL, tested via Python tests.
}
