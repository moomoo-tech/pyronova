use pyo3::prelude::*;
use pyo3::types::{
    PyBool, PyByteArray, PyBytes, PyDict, PyFloat, PyInt, PyList, PyMapping, PyNone, PyString,
    PyTuple,
};
use std::collections::HashSet;
use std::fmt::{self, Write as _};

const MAX_DEPTH: usize = 256;
const SIGNAL_CHECK_INTERVAL: usize = 1000;

enum PathSegment {
    Key(String),
    Index(usize),
}

impl fmt::Display for PathSegment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Key(k) => write!(f, ".{}", k),
            Self::Index(i) => write!(f, "[{}]", i),
        }
    }
}

#[derive(Debug)]
pub(crate) enum ErrorReason {
    CircularReference,
    MaxDepthExceeded(usize),
    UnsupportedType(String),
    UnsupportedDictKey(String),
    InvalidFloat(f64),
    Interrupted,
}

/// Structured error carrying the precise JSON Path where failure occurred.
#[derive(Debug)]
pub(crate) struct PyJsonError {
    path: String,
    reason: ErrorReason,
}

impl fmt::Display for PyJsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.reason {
            ErrorReason::CircularReference => {
                write!(f, "Circular reference detected at {}", self.path)
            }
            ErrorReason::MaxDepthExceeded(d) => {
                write!(
                    f,
                    "Maximum nesting depth exceeded at {}: {} > {}",
                    self.path, d, MAX_DEPTH
                )
            }
            ErrorReason::UnsupportedType(t) => {
                write!(
                    f,
                    "Cannot serialize Python type to JSON at {}: {}",
                    self.path, t
                )
            }
            ErrorReason::UnsupportedDictKey(t) => {
                write!(f, "Unsupported dict key type at {}: {}", self.path, t)
            }
            ErrorReason::InvalidFloat(v) => {
                write!(
                    f,
                    "JSON does not support NaN or Infinity at {}: {}",
                    self.path, v
                )
            }
            ErrorReason::Interrupted => {
                write!(f, "Serialization interrupted by signal at {}", self.path)
            }
        }
    }
}

impl std::error::Error for PyJsonError {}

fn type_name_of(obj: &Bound<'_, pyo3::PyAny>) -> String {
    obj.get_type()
        .name()
        .map_or_else(|_| "<unknown>".to_string(), |n| n.to_string())
}

pub(crate) struct JsonContext {
    visited: HashSet<usize>,
    path: Vec<PathSegment>,
    element_count: usize,
}

impl JsonContext {
    fn new() -> Self {
        Self {
            visited: HashSet::new(),
            path: Vec::new(),
            element_count: 0,
        }
    }

    fn current_path(&self) -> String {
        let mut s = String::from("$");
        for seg in &self.path {
            write!(s, "{}", seg).ok();
        }
        s
    }

    fn err(&self, reason: ErrorReason) -> PyJsonError {
        PyJsonError {
            path: self.current_path(),
            reason,
        }
    }

    fn maybe_check_signals(&mut self, py: Python<'_>) -> Result<(), PyJsonError> {
        self.element_count += 1;
        if self.element_count.is_multiple_of(SIGNAL_CHECK_INTERVAL) {
            py.check_signals()
                .map_err(|_| self.err(ErrorReason::Interrupted))?;
        }
        Ok(())
    }

    fn serialize(
        &mut self,
        obj: &Bound<'_, pyo3::PyAny>,
    ) -> Result<serde_json::Value, PyJsonError> {
        if self.path.len() > MAX_DEPTH {
            return Err(self.err(ErrorReason::MaxDepthExceeded(self.path.len())));
        }

        let ptr = obj.as_ptr() as usize;
        if !self.visited.insert(ptr) {
            return Err(self.err(ErrorReason::CircularReference));
        }

        let result = self.parse_node(obj);

        self.visited.remove(&ptr);
        result
    }

    fn parse_node(
        &mut self,
        obj: &Bound<'_, pyo3::PyAny>,
    ) -> Result<serde_json::Value, PyJsonError> {
        if obj.is_none() {
            return Ok(serde_json::Value::Null);
        }

        // cast_exact: match PyBool before PyInt to prevent subclass confusion
        if let Ok(b) = obj.cast_exact::<PyBool>() {
            return Ok(serde_json::Value::Bool(b.is_true()));
        }

        if let Ok(i) = obj.cast::<PyInt>() {
            if let Ok(v) = i.extract::<i64>() {
                return Ok(serde_json::Value::Number(v.into()));
            }
            return Ok(serde_json::Value::String(i.to_string()));
        }

        if let Ok(f) = obj.cast::<PyFloat>() {
            let v = f.value();
            if v.is_nan() || v.is_infinite() {
                return Err(self.err(ErrorReason::InvalidFloat(v)));
            }
            if let Some(n) = serde_json::Number::from_f64(v) {
                return Ok(serde_json::Value::Number(n));
            }
        }

        // String MUST be checked before iterable (str implements Sequence in Python)
        if let Ok(s) = obj.cast::<PyString>() {
            return Ok(serde_json::Value::String(s.to_string_lossy().into_owned()));
        }

        // Fast path: PyDict
        if let Ok(dict) = obj.cast::<PyDict>() {
            return self.serialize_dict_pairs(dict.iter(), dict.len(), obj.py());
        }

        // Fast path: PyList
        if let Ok(list) = obj.cast::<PyList>() {
            return self.serialize_seq(list.iter().map(Ok), list.len(), obj.py());
        }

        // Fast path: PyTuple
        if let Ok(tuple) = obj.cast::<PyTuple>() {
            return self.serialize_seq(tuple.iter().map(Ok), tuple.len(), obj.py());
        }

        // Reject bytes/bytearray — they implement Sequence but should not become int arrays
        if obj.cast::<PyBytes>().is_ok() || obj.cast::<PyByteArray>().is_ok() {
            return Err(self.err(ErrorReason::UnsupportedType(type_name_of(obj))));
        }

        // Duck type: PyMapping (defaultdict, OrderedDict, custom Mapping subclasses)
        if let Ok(mapping) = obj.cast::<PyMapping>() {
            match mapping.items() {
                Ok(items) => {
                    let len = mapping.len().unwrap_or(0);
                    return self.serialize_mapping_items(&items, len, obj.py());
                }
                Err(_) => {
                    return Err(self.err(ErrorReason::UnsupportedType(
                        "Mapping.items() raised an exception".into(),
                    )));
                }
            }
        }

        // Duck type: any iterable (deque, generators, etc.) — O(1) per step
        if let Ok(iter) = obj.try_iter() {
            return self.serialize_seq(iter.map(|r| r.map_err(|_| ())), 0, obj.py());
        }

        Err(self.err(ErrorReason::UnsupportedType(type_name_of(obj))))
    }

    /// Serialize an iterator of (key, value) pairs into a JSON object.
    fn serialize_dict_pairs<'py>(
        &mut self,
        iter: impl Iterator<Item = (Bound<'py, pyo3::PyAny>, Bound<'py, pyo3::PyAny>)>,
        capacity: usize,
        py: Python<'py>,
    ) -> Result<serde_json::Value, PyJsonError> {
        let mut map = serde_json::Map::with_capacity(capacity);
        for (k, v) in iter {
            self.maybe_check_signals(py)?;
            let key_str = self.coerce_dict_key(&k)?;

            // Push path, serialize, pop — then recover key from the segment to avoid clone
            self.path.push(PathSegment::Key(key_str));
            let val = self.serialize(&v)?;
            let key_str = match self.path.pop() {
                Some(PathSegment::Key(k)) => k,
                _ => unreachable!(),
            };

            // Last-writer-wins: matches Python json.dumps behavior
            map.insert(key_str, val);
        }
        Ok(serde_json::Value::Object(map))
    }

    /// Serialize items() from a generic PyMapping (list of 2-tuples).
    fn serialize_mapping_items<'py>(
        &mut self,
        items: &Bound<'py, PyList>,
        capacity: usize,
        py: Python<'py>,
    ) -> Result<serde_json::Value, PyJsonError> {
        let mut map = serde_json::Map::with_capacity(capacity);
        for item in items.iter() {
            self.maybe_check_signals(py)?;

            let k = item
                .get_item(0)
                .map_err(|_| self.err(ErrorReason::UnsupportedType("bad mapping item".into())))?;
            let v = item
                .get_item(1)
                .map_err(|_| self.err(ErrorReason::UnsupportedType("bad mapping item".into())))?;

            let key_str = self.coerce_dict_key(&k)?;

            self.path.push(PathSegment::Key(key_str));
            let val = self.serialize(&v)?;
            let key_str = match self.path.pop() {
                Some(PathSegment::Key(k)) => k,
                _ => unreachable!(),
            };

            map.insert(key_str, val);
        }
        Ok(serde_json::Value::Object(map))
    }

    /// Serialize a fallible sequence iterator into a JSON array.
    /// Infallible iterators (PyList, PyTuple) pass `.map(Ok)`.
    fn serialize_seq<'py>(
        &mut self,
        iter: impl Iterator<Item = Result<Bound<'py, pyo3::PyAny>, ()>>,
        capacity: usize,
        py: Python<'py>,
    ) -> Result<serde_json::Value, PyJsonError> {
        let mut arr = Vec::with_capacity(capacity);
        for (idx, item_result) in iter.enumerate() {
            self.maybe_check_signals(py)?;
            let item = item_result.map_err(|_| {
                self.err(ErrorReason::UnsupportedType(
                    "iterator yielded an error".into(),
                ))
            })?;
            self.path.push(PathSegment::Index(idx));
            arr.push(self.serialize(&item)?);
            self.path.pop();
        }
        Ok(serde_json::Value::Array(arr))
    }

    /// Coerce a Python dict key to a JSON string.
    /// Supports: str, bool, int, float, None (matching Python json.dumps).
    fn coerce_dict_key(&self, k: &Bound<'_, pyo3::PyAny>) -> Result<String, PyJsonError> {
        if let Ok(py_str) = k.cast::<PyString>() {
            return Ok(py_str.to_string_lossy().into_owned());
        }

        // bool before int (bool is int subclass in Python)
        if let Ok(b) = k.cast_exact::<PyBool>() {
            return Ok(if b.is_true() {
                "true".to_string()
            } else {
                "false".to_string()
            });
        }

        if k.cast::<PyInt>().is_ok() {
            return Ok(k.to_string());
        }

        if let Ok(f) = k.cast::<PyFloat>() {
            let fv = f.value();
            if fv.is_nan() || fv.is_infinite() {
                return Err(self.err(ErrorReason::InvalidFloat(fv)));
            }
            return Ok(fv.to_string());
        }

        if k.cast::<PyNone>().is_ok() {
            return Ok("null".to_string());
        }

        Err(self.err(ErrorReason::UnsupportedDictKey(type_name_of(k))))
    }
}

/// Convert a Python object to a serde_json::Value.
/// Errors include JSON Path context (e.g. `$.users[2].score`).
pub(crate) fn py_to_json_value(
    obj: &pyo3::Bound<'_, pyo3::PyAny>,
) -> Result<serde_json::Value, PyJsonError> {
    let mut ctx = JsonContext::new();
    ctx.serialize(obj)
}
