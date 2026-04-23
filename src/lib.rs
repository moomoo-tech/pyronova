#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod app;
mod body_stream;
mod compression;
mod db;
mod db_bridge;
mod grpc;
mod handlers;
mod interp;
mod json;
#[cfg(feature = "leak_detect")]
mod leak_detect;
mod logging;
mod main_bridge;
mod monitor;
mod pyronova_request_type;
mod response;
mod router;
mod state;
mod static_fs;
mod stream;
mod tpc;
mod tls;
mod types;
mod websocket;
mod worker;

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
fn pyronova_request_counts() -> (usize, usize) {
    (
        pyronova_request_type::ALLOC_COUNT.load(std::sync::atomic::Ordering::Relaxed),
        pyronova_request_type::DEALLOC_COUNT.load(std::sync::atomic::Ordering::Relaxed),
    )
}

#[cfg(feature = "leak_detect")]
#[pyo3::pyfunction]
fn pyronova_slot_rc_report() -> String {
    pyronova_request_type::slot_rc_report()
}

#[pymodule]
fn engine(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<app::PyronovaApp>()?;
    m.add_class::<types::PyronovaRequest>()?;
    m.add_class::<types::PyronovaResponse>()?;
    m.add_class::<websocket::PyronovaWebSocket>()?;
    m.add_class::<state::SharedState>()?;
    m.add_class::<stream::PyronovaStream>()?;
    m.add_class::<body_stream::PyronovaBodyStream>()?;
    m.add_class::<db::PgPool>()?;
    m.add_class::<db::PgCursor>()?;
    m.add_function(pyo3::wrap_pyfunction!(monitor::get_gil_metrics, m)?)?;
    m.add_function(pyo3::wrap_pyfunction!(logging::init_logger, m)?)?;
    m.add_function(pyo3::wrap_pyfunction!(logging::emit_python_log, m)?)?;
    m.add_function(pyo3::wrap_pyfunction!(workrequest_counts, m)?)?;
    #[cfg(feature = "leak_detect")]
    m.add_function(pyo3::wrap_pyfunction!(pyronova_request_counts, m)?)?;
    #[cfg(feature = "leak_detect")]
    m.add_function(pyo3::wrap_pyfunction!(pyronova_slot_rc_report, m)?)?;
    #[cfg(feature = "leak_detect")]
    m.add_function(pyo3::wrap_pyfunction!(leak_detect_dump, m)?)?;
    Ok(())
}
