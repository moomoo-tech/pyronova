mod app;
mod handlers;
mod interp;
mod json;
mod response;
mod router;
mod static_fs;
mod types;
mod websocket;

use pyo3::prelude::*;

#[pymodule]
fn engine(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<app::SkyApp>()?;
    m.add_class::<types::SkyRequest>()?;
    m.add_class::<types::SkyResponse>()?;
    m.add_class::<websocket::SkyWebSocket>()?;
    Ok(())
}
