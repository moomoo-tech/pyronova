//! Python runtime boundary — sub-interpreter management + streaming
//! glue between hyper and Python.
//!
//! Grouping rationale: these three modules hold the densest
//! concentration of `unsafe` + `pyo3::ffi::*` in the codebase.
//! Everything else in the tree either uses PyO3's safe bindings
//! (`Python::attach`, `Py<PyAny>`, #[pyclass] getters) or has no
//! PyO3 contact at all. Physically grouping the unsafe-heavy files
//! makes the FFI boundary easy to audit and isolate.
//!
//! - `interp`: sub-interpreter worker / GIL / tstate rebinding.
//!   Most of the `unsafe` in the codebase lives here.
//! - `body_stream`: hyper Request body → Python channel. Used by
//!   `stream=True` routes to feed upload data incrementally into
//!   a Python async generator.
//! - `stream`: Python channel → hyper Response body. Backs
//!   Server-Sent Events (SSE) responses.
//!
//! Exports the previous crate-root module paths by re-exporting
//! as `pub(crate) use`, so existing `crate::interp::X`-style call
//! sites keep compiling with `crate::python::interp::X`.

pub(crate) mod body_stream;
pub(crate) mod interp;
pub(crate) mod stream;
