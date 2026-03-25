#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod app;
mod handlers;
mod interp;
mod json;
mod response;
mod router;
mod static_fs;
mod types;
mod monitor;
mod state;
mod stream;
mod websocket;

use pyo3::prelude::*;

#[pymodule]
fn engine(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<app::SkyApp>()?;
    m.add_class::<types::SkyRequest>()?;
    m.add_class::<types::SkyResponse>()?;
    m.add_class::<websocket::SkyWebSocket>()?;
    m.add_class::<state::SharedState>()?;
    m.add_class::<stream::SkyStream>()?;
    m.add_function(pyo3::wrap_pyfunction!(monitor::get_gil_metrics, m)?)?;
    m.add_function(pyo3::wrap_pyfunction!(enable_request_logging, m)?)?;
    m.add_function(pyo3::wrap_pyfunction!(set_cors_origin, m)?)?;
    Ok(())
}

#[pyfunction]
fn enable_request_logging(enabled: bool) {
    interp::REQUEST_LOGGING.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

#[pyfunction]
fn set_cors_origin(origin: String) {
    let _ = interp::CORS_ORIGIN.set(origin);
}
