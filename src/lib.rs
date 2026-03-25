#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod app;
mod handlers;
mod interp;
mod json;
mod monitor;
mod response;
mod router;
mod state;
mod static_fs;
mod stream;
mod types;
mod websocket;

use pyo3::prelude::*;

#[pymodule]
fn engine(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<app::PyreApp>()?;
    m.add_class::<types::PyreRequest>()?;
    m.add_class::<types::PyreResponse>()?;
    m.add_class::<websocket::PyreWebSocket>()?;
    m.add_class::<state::SharedState>()?;
    m.add_class::<stream::PyreStream>()?;
    m.add_function(pyo3::wrap_pyfunction!(monitor::get_gil_metrics, m)?)?;
    Ok(())
}
