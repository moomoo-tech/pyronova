#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod app;
mod compression;
mod handlers;
mod interp;
mod json;
#[cfg(feature = "leak_detect")]
mod leak_detect;
mod logging;
mod monitor;
mod pyre_request_type;
mod response;
mod router;
mod state;
mod static_fs;
mod stream;
mod tls;
mod types;
mod websocket;

use pyo3::prelude::*;

#[cfg(feature = "leak_detect")]
#[pyo3::pyfunction]
fn leak_detect_dump() {
    leak_detect::dump_to_stderr();
}

#[pyo3::pyfunction]
fn workrequest_counts() -> (u64, u64) {
    (
        interp::WorkRequest::created_count(),
        interp::WorkRequest::dropped_count(),
    )
}

#[cfg(feature = "leak_detect")]
#[pyo3::pyfunction]
fn pyre_request_counts() -> (usize, usize) {
    (
        pyre_request_type::ALLOC_COUNT.load(std::sync::atomic::Ordering::Relaxed),
        pyre_request_type::DEALLOC_COUNT.load(std::sync::atomic::Ordering::Relaxed),
    )
}

#[cfg(feature = "leak_detect")]
#[pyo3::pyfunction]
fn pyre_slot_rc_report() -> String {
    pyre_request_type::slot_rc_report()
}

#[pymodule]
fn engine(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<app::PyreApp>()?;
    m.add_class::<types::PyreRequest>()?;
    m.add_class::<types::PyreResponse>()?;
    m.add_class::<websocket::PyreWebSocket>()?;
    m.add_class::<state::SharedState>()?;
    m.add_class::<stream::PyreStream>()?;
    m.add_function(pyo3::wrap_pyfunction!(monitor::get_gil_metrics, m)?)?;
    m.add_function(pyo3::wrap_pyfunction!(logging::init_logger, m)?)?;
    m.add_function(pyo3::wrap_pyfunction!(logging::emit_python_log, m)?)?;
    m.add_function(pyo3::wrap_pyfunction!(workrequest_counts, m)?)?;
    #[cfg(feature = "leak_detect")]
    m.add_function(pyo3::wrap_pyfunction!(pyre_request_counts, m)?)?;
    #[cfg(feature = "leak_detect")]
    m.add_function(pyo3::wrap_pyfunction!(pyre_slot_rc_report, m)?)?;
    #[cfg(feature = "leak_detect")]
    m.add_function(pyo3::wrap_pyfunction!(leak_detect_dump, m)?)?;
    Ok(())
}
