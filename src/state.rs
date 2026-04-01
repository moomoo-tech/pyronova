//! SharedState — cross-sub-interpreter state sharing via DashMap.
//!
//! All sub-interpreters share the same Arc<DashMap> in Rust memory.
//! Python code uses `app.state["key"] = value` / `app.state["key"]`.
//! Values stored as `bytes::Bytes` (ref-counted, zero-cost clone).

use std::sync::Arc;

use bytes::Bytes;
use dashmap::DashMap;
use pyo3::prelude::*;

/// High-concurrency shared key-value store backed by DashMap.
///
/// Thread-safe, lock-free reads for different keys, nanosecond latency.
/// All sub-interpreters share the same underlying DashMap via Arc.
/// Values are `Bytes` — clone is atomic refcount bump, not deep copy.
#[pyclass]
pub(crate) struct SharedState {
    inner: Arc<DashMap<String, Bytes>>,
}

impl SharedState {
    /// Create a new SharedState with the given Arc (for sharing across workers).
    pub fn with_inner(inner: Arc<DashMap<String, Bytes>>) -> Self {
        SharedState { inner }
    }
}

#[pymethods]
impl SharedState {
    #[new]
    fn new() -> Self {
        SharedState {
            inner: Arc::new(DashMap::new()),
        }
    }

    /// Set a string value.
    fn set(&self, key: String, value: String) {
        self.inner.insert(key, Bytes::from(value.into_bytes()));
    }

    /// Get a string value. Returns None if key doesn't exist.
    fn get(&self, key: &str) -> Option<String> {
        self.inner
            .get(key)
            .and_then(|v| std::str::from_utf8(v.value()).ok().map(|s| s.to_string()))
    }

    /// Set raw bytes value.
    fn set_bytes(&self, key: String, value: Vec<u8>) {
        self.inner.insert(key, Bytes::from(value));
    }

    /// Get raw bytes value (zero-copy clone via Bytes refcount).
    fn get_bytes(&self, key: &str) -> Option<Vec<u8>> {
        self.inner.get(key).map(|v| v.value().to_vec())
    }

    /// Delete a key. Returns True if it existed.
    fn delete(&self, key: &str) -> bool {
        self.inner.remove(key).is_some()
    }

    /// Get all keys.
    fn keys(&self) -> Vec<String> {
        self.inner.iter().map(|e| e.key().clone()).collect()
    }

    /// Number of entries.
    fn __len__(&self) -> usize {
        self.inner.len()
    }

    /// Check if key exists.
    fn __contains__(&self, key: &str) -> bool {
        self.inner.contains_key(key)
    }

    /// dict-like: state["key"] = "value"
    fn __setitem__(&self, key: String, value: String) {
        self.set(key, value);
    }

    /// dict-like: state["key"]
    fn __getitem__(&self, key: &str) -> PyResult<String> {
        self.get(key)
            .ok_or_else(|| pyo3::exceptions::PyKeyError::new_err(key.to_string()))
    }

    /// dict-like: del state["key"]
    fn __delitem__(&self, key: &str) -> PyResult<()> {
        if self.delete(key) {
            Ok(())
        } else {
            Err(pyo3::exceptions::PyKeyError::new_err(key.to_string()))
        }
    }

    fn __repr__(&self) -> String {
        format!("SharedState({} keys)", self.inner.len())
    }
}
