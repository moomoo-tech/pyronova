use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyList, PyString};

pub(crate) fn py_to_json_value(obj: &pyo3::Bound<'_, pyo3::PyAny>) -> Result<serde_json::Value, String> {
    if obj.is_none() {
        return Ok(serde_json::Value::Null);
    }
    if let Ok(b) = obj.downcast::<PyBool>() {
        return Ok(serde_json::Value::Bool(b.is_true()));
    }
    if let Ok(i) = obj.downcast::<PyInt>() {
        if let Ok(v) = i.extract::<i64>() {
            return Ok(serde_json::Value::Number(v.into()));
        }
    }
    if let Ok(f) = obj.downcast::<PyFloat>() {
        if let Ok(v) = f.extract::<f64>() {
            if let Some(n) = serde_json::Number::from_f64(v) {
                return Ok(serde_json::Value::Number(n));
            }
        }
    }
    if let Ok(s) = obj.downcast::<PyString>() {
        return Ok(serde_json::Value::String(s.to_string()));
    }
    if let Ok(list) = obj.downcast::<PyList>() {
        let mut arr = Vec::with_capacity(list.len());
        for item in list.iter() {
            arr.push(py_to_json_value(&item)?);
        }
        return Ok(serde_json::Value::Array(arr));
    }
    if let Ok(dict) = obj.downcast::<PyDict>() {
        let mut map = serde_json::Map::with_capacity(dict.len());
        for (k, v) in dict.iter() {
            let key = k.extract::<String>().map_err(|e| e.to_string())?;
            map.insert(key, py_to_json_value(&v)?);
        }
        return Ok(serde_json::Value::Object(map));
    }
    Ok(serde_json::Value::String(
        obj.str().map_err(|e| e.to_string())?.to_string(),
    ))
}

// Note: py_to_json_value requires a Python interpreter, so it's tested
// via Python integration tests rather than cargo test.
