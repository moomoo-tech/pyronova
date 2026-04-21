use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, OnceLock};

use bytes::Bytes;
use pyo3::prelude::*;

// ---------------------------------------------------------------------------
// PyronovaRequest
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

#[pyclass(frozen, skip_from_py_object, name = "Request")]
pub(crate) struct PyronovaRequest {
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
    /// For streaming routes (`stream=True`), this holds the feeder channel's
    /// receiver end, shared across all clones of this request so the
    /// handler (which receives a clone from `call_handler_with_hooks`) can
    /// take ownership. The first `.stream` access wins; subsequent calls
    /// return None. Stored as raw receiver (not `Py<PyronovaBodyStream>`) to
    /// keep `drop_in_place::<PyronovaRequest>` free of `_Py_Dealloc` — that
    /// would break `cargo test` linking for the pure-Rust unit tests.
    pub(crate) body_stream_rx:
        Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Receiver<crate::body_stream::ChunkMsg>>>>,
    /// Cached parse of the query string. `form_urlencoded::parse + collect`
    /// costs ~100-200 ns for a two-param query and building a fresh
    /// Python dict on top is another ~500 ns. OnceLock matches the
    /// headers_cache pattern: parse once, return ref on subsequent
    /// accesses.
    pub(crate) query_cache: OnceLock<HashMap<String, String>>,
}

/// Manual Clone: OnceLock doesn't impl Clone, so we reset the cache on clone.
/// Cloned requests lazily recompute headers if accessed.
impl Clone for PyronovaRequest {
    fn clone(&self) -> Self {
        // body_stream_rx is a one-shot channel — it can't be cloned. Clones
        // get an empty stream slot; the original passed to the handler keeps
        // the receiver. Since the framework only streams on the main request
        // copy (before_request / after_request hooks run under the same
        // sky_req before the clone cascade), this is the correct semantics.
        Self {
            method: self.method.clone(),
            path: self.path.clone(),
            params: self.params.clone(),
            query: self.query.clone(),
            headers_source: self.headers_source.clone(),
            headers_cache: OnceLock::new(),
            client_ip_addr: self.client_ip_addr,
            body_bytes: self.body_bytes.clone(),
            body_stream_rx: Arc::clone(&self.body_stream_rx),
            query_cache: OnceLock::new(),
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

impl PyronovaRequest {
    /// Resolve headers to HashMap (lazy for Raw, immediate for Converted).
    pub(crate) fn resolved_headers(&self) -> &HashMap<String, String> {
        self.headers_cache
            .get_or_init(|| match &self.headers_source {
                LazyHeaders::Raw(hm) => extract_headers(hm),
                LazyHeaders::Converted(m) => m.clone(),
            })
    }
}

#[pymethods]
impl PyronovaRequest {
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

    /// Streaming body iterator. Only populated on routes registered with
    /// `stream=True`; returns `None` otherwise so code that doesn't opt-in
    /// never sees a stream object.
    ///
    /// **Consumed on first access.** The receiver is taken out of the
    /// shared slot, so a second call to `req.stream` in the same request
    /// lifecycle returns `None`. Before/after hooks that clone the request
    /// and read `.stream` will steal chunks from the handler — that's the
    /// caller's bug, not ours.
    ///
    /// Usage in a `@app.post(..., gil=True, stream=True)` handler:
    ///
    /// ```python
    /// for chunk in req.stream:
    ///     process(chunk)
    /// ```
    #[getter]
    fn stream(
        &self,
        py: Python<'_>,
    ) -> PyResult<Option<Py<crate::body_stream::PyronovaBodyStream>>> {
        let rx = { self.body_stream_rx.lock().unwrap().take() };
        match rx {
            Some(rx) => Ok(Some(Py::new(
                py,
                crate::body_stream::PyronovaBodyStream::new(rx),
            )?)),
            None => Ok(None),
        }
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
        let parsed: serde_json::Value = serde_json::from_slice(&self.body_bytes).map_err(|e| {
            pyo3::exceptions::PyValueError::new_err(format!("JSON parse error: {e}"))
        })?;
        pythonize::pythonize(py, &parsed)
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("pythonize error: {e}")))
    }

    #[getter]
    fn query_params(&self) -> HashMap<String, String> {
        // Parse once, reuse on subsequent accesses. Handlers that read
        // `req.query_params.get("a")` and `req.query_params.get("b")`
        // previously parsed the query string twice.
        self.query_cache
            .get_or_init(|| {
                form_urlencoded::parse(self.query.as_bytes())
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect()
            })
            .clone()
    }
}

// ---------------------------------------------------------------------------
// PyronovaResponse
// ---------------------------------------------------------------------------

#[pyclass(frozen, name = "Response")]
pub(crate) struct PyronovaResponse {
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
impl PyronovaResponse {
    #[new]
    #[pyo3(signature = (body, status_code=200, content_type=None, headers=None))]
    fn new(
        body: Py<PyAny>,
        status_code: u16,
        content_type: Option<String>,
        headers: Option<HashMap<String, String>>,
    ) -> Self {
        PyronovaResponse {
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
        let req = PyronovaRequest {
            method: Arc::from("GET"),
            path: Arc::from("/search"),
            params: Vec::new(),
            query: "q=hello+world&page=2&lang=en".to_string(),
            headers_source: LazyHeaders::Raw(hyper::HeaderMap::new()),
            headers_cache: std::sync::OnceLock::new(),
            client_ip_addr: IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            body_bytes: Bytes::new(),
            body_stream_rx: Arc::new(std::sync::Mutex::new(None)),
            query_cache: OnceLock::new(),
        };
        let qp = req.query_params();
        assert_eq!(qp["q"], "hello world");
        assert_eq!(qp["page"], "2");
        assert_eq!(qp["lang"], "en");
    }

    #[test]
    fn query_params_empty() {
        let req = PyronovaRequest {
            method: Arc::from("GET"),
            path: Arc::from("/"),
            params: Vec::new(),
            query: "".to_string(),
            headers_source: LazyHeaders::Raw(hyper::HeaderMap::new()),
            headers_cache: std::sync::OnceLock::new(),
            client_ip_addr: IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            body_bytes: Bytes::new(),
            body_stream_rx: Arc::new(std::sync::Mutex::new(None)),
            query_cache: OnceLock::new(),
        };
        assert!(req.query_params().is_empty());
    }

    #[test]
    fn query_params_percent_encoded() {
        let req = PyronovaRequest {
            method: Arc::from("GET"),
            path: Arc::from("/"),
            params: Vec::new(),
            query: "name=%E4%B8%AD%E6%96%87".to_string(),
            headers_source: LazyHeaders::Raw(hyper::HeaderMap::new()),
            headers_cache: std::sync::OnceLock::new(),
            client_ip_addr: IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            body_bytes: Bytes::new(),
            body_stream_rx: Arc::new(std::sync::Mutex::new(None)),
            query_cache: OnceLock::new(),
        };
        assert_eq!(req.query_params()["name"], "中文");
    }

    // Note: text() and json() require Python GIL, tested via Python tests.
}
