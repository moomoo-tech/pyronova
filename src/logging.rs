//! Pyre logging engine — zero-cost tracing with non-blocking I/O.
//!
//! Provides:
//! - `init_logger`: configures tracing-subscriber with non-blocking writer
//! - `emit_python_log`: receives Python `logging` calls via FFI, routes to tracing
//!
//! Key: uses `tracing-appender::non_blocking` to avoid StdoutLock contention.
//! Without this, 220k QPS access log would starve Tokio worker threads on
//! the global stdout mutex.

use std::sync::OnceLock;

use pyo3::prelude::*;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Global guard for the non-blocking writer's background I/O thread.
/// If dropped, the background thread stops and buffered logs are lost.
/// OnceLock ensures it lives for the entire process lifetime.
static NB_GUARD: OnceLock<tracing_appender::non_blocking::WorkerGuard> = OnceLock::new();

/// Initialize the Rust tracing engine. Called once at Pyre startup.
///
/// - `level`: filter string — "OFF", "ERROR", "WARN", "INFO", "DEBUG", "TRACE"
/// - `access_log`: if false, suppresses all `pyre::access` target logs
/// - `format`: "json" for structured output, anything else for human-readable text
#[pyfunction]
#[pyo3(signature = (level, access_log, format))]
pub fn init_logger(level: String, access_log: bool, format: String) -> PyResult<()> {
    let mut filter = EnvFilter::new(&level);

    // Suppress access log target when disabled
    if !access_log {
        if let Ok(directive) = "pyre::access=off".parse() {
            filter = filter.add_directive(directive);
        }
    }

    // Non-blocking writer: all log I/O happens on a dedicated background thread.
    // Tokio workers never block on stdout — they just push into an MPSC channel.
    let (nb_writer, guard) = tracing_appender::non_blocking(std::io::stderr());
    let _ = NB_GUARD.set(guard);

    let result = if format.to_lowercase() == "json" {
        tracing_subscriber::registry()
            .with(filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(nb_writer)
                    .json(),
            )
            .try_init()
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(nb_writer)
                    .with_target(true)
                    .with_ansi(true),
            )
            .try_init()
    };

    if result.is_ok() {
        tracing::info!(
            target: "pyre::server",
            level = %level,
            access_log = access_log,
            format = %format,
            "Pyre tracing engine initialized"
        );
    }
    // Silently ignore if already initialized (hot reload, tests)

    Ok(())
}

/// Receive a Python logging record and route it through Rust tracing.
///
/// Called from `PyreRustHandler.emit()` in each interpreter (main + sub-interpreters).
/// The actual filtering is done by `EnvFilter` — Python side sets level=DEBUG
/// to let everything through, Rust decides what to keep.
#[pyfunction]
#[pyo3(signature = (level, name, message, pathname, lineno, worker_id=None))]
pub fn emit_python_log(
    level: String,
    name: String,
    message: String,
    pathname: String,
    lineno: u32,
    worker_id: Option<usize>,
) -> PyResult<()> {
    let wid = worker_id.unwrap_or(0);

    // Dispatch to compile-time tracing macros via match.
    // Each branch is a separate static callsite — EnvFilter can skip at near-zero cost.
    match level.as_str() {
        "DEBUG" => {
            tracing::debug!(
                target: "pyre::app",
                worker = wid,
                logger = %name,
                file = %pathname,
                line = lineno,
                "{}", message
            );
        }
        "INFO" => {
            tracing::info!(
                target: "pyre::app",
                worker = wid,
                logger = %name,
                file = %pathname,
                line = lineno,
                "{}", message
            );
        }
        "WARNING" => {
            tracing::warn!(
                target: "pyre::app",
                worker = wid,
                logger = %name,
                file = %pathname,
                line = lineno,
                "{}", message
            );
        }
        "ERROR" | "CRITICAL" => {
            tracing::error!(
                target: "pyre::app",
                worker = wid,
                logger = %name,
                file = %pathname,
                line = lineno,
                "{}", message
            );
        }
        _ => {
            tracing::trace!(
                target: "pyre::app",
                worker = wid,
                logger = %name,
                file = %pathname,
                line = lineno,
                "{}", message
            );
        }
    }

    Ok(())
}
