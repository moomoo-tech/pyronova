use std::collections::HashMap;

use bytes::Bytes;
use pyo3::prelude::*;

// ---------------------------------------------------------------------------
// PyreRequest
// ---------------------------------------------------------------------------

#[pyclass(frozen, skip_from_py_object)]
#[derive(Clone)]
pub(crate) struct PyreRequest {
    #[pyo3(get)]
    pub(crate) method: String,
    #[pyo3(get)]
    pub(crate) path: String,
    #[pyo3(get)]
    pub(crate) params: HashMap<String, String>,
    #[pyo3(get)]
    pub(crate) query: String,
    #[pyo3(get)]
    pub(crate) headers: HashMap<String, String>,
    pub(crate) body_bytes: Vec<u8>,
}

#[pymethods]
impl PyreRequest {
    #[getter]
    fn body(&self) -> &[u8] {
        &self.body_bytes
    }

    fn text(&self) -> PyResult<String> {
        String::from_utf8(self.body_bytes.clone())
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))
    }

    fn json<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, pyo3::PyAny>> {
        let text = self.text()?;
        let json_mod = py.import("json")?;
        json_mod.call_method1("loads", (text,))
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
        let val = String::from_utf8_lossy(value.as_bytes()).to_string();
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
            method: "GET".to_string(),
            path: "/search".to_string(),
            params: HashMap::new(),
            query: "q=hello+world&page=2&lang=en".to_string(),
            headers: HashMap::new(),
            body_bytes: Vec::new(),
        };
        let qp = req.query_params();
        assert_eq!(qp["q"], "hello world");
        assert_eq!(qp["page"], "2");
        assert_eq!(qp["lang"], "en");
    }

    #[test]
    fn query_params_empty() {
        let req = PyreRequest {
            method: "GET".to_string(),
            path: "/".to_string(),
            params: HashMap::new(),
            query: "".to_string(),
            headers: HashMap::new(),
            body_bytes: Vec::new(),
        };
        assert!(req.query_params().is_empty());
    }

    #[test]
    fn query_params_percent_encoded() {
        let req = PyreRequest {
            method: "GET".to_string(),
            path: "/".to_string(),
            params: HashMap::new(),
            query: "name=%E4%B8%AD%E6%96%87".to_string(),
            headers: HashMap::new(),
            body_bytes: Vec::new(),
        };
        assert_eq!(req.query_params()["name"], "中文");
    }

    // Note: text() and json() require Python GIL, tested via Python tests.
}
