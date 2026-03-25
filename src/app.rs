use std::sync::Arc;

use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::Request;
use hyper_util::rt::TokioIo;
use hyper_util::server::conn::auto::Builder as AutoBuilder;
use pyo3::prelude::*;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio::runtime::Builder as RuntimeBuilder;
use tokio::signal;

use crate::handlers::{handle_request, handle_request_subinterp};
use crate::interp;
use crate::router::{FrozenRoutes, MutableRoutes, RouteTable};
use crate::state::SharedState;
use crate::websocket;

#[pyclass]
pub(crate) struct SkyApp {
    routes: MutableRoutes,
    script_path: Option<String>,
    shared_state: Arc<dashmap::DashMap<String, Vec<u8>>>,
}

#[pymethods]
impl SkyApp {
    #[new]
    fn new() -> Self {
        SkyApp {
            routes: Arc::new(parking_lot::RwLock::new(RouteTable::new())),
            script_path: None,
            shared_state: Arc::new(dashmap::DashMap::new()),
        }
    }

    /// Access the shared state (cross-sub-interpreter, nanosecond latency).
    #[getter]
    fn state(&self) -> SharedState {
        SharedState::with_inner(Arc::clone(&self.shared_state))
    }

    #[pyo3(signature = (path, handler, gil=false))]
    fn get(&mut self, path: &str, handler: Py<PyAny>, gil: bool, py: Python<'_>) -> PyResult<()> {
        let name = handler.getattr(py, "__name__")?.extract::<String>(py)?;
        self.add_route("GET", path, handler, name, gil, py)
    }

    #[pyo3(signature = (path, handler, gil=false))]
    fn post(&mut self, path: &str, handler: Py<PyAny>, gil: bool, py: Python<'_>) -> PyResult<()> {
        let name = handler.getattr(py, "__name__")?.extract::<String>(py)?;
        self.add_route("POST", path, handler, name, gil, py)
    }

    #[pyo3(signature = (path, handler, gil=false))]
    fn put(&mut self, path: &str, handler: Py<PyAny>, gil: bool, py: Python<'_>) -> PyResult<()> {
        let name = handler.getattr(py, "__name__")?.extract::<String>(py)?;
        self.add_route("PUT", path, handler, name, gil, py)
    }

    #[pyo3(signature = (path, handler, gil=false))]
    fn delete(&mut self, path: &str, handler: Py<PyAny>, gil: bool, py: Python<'_>) -> PyResult<()> {
        let name = handler.getattr(py, "__name__")?.extract::<String>(py)?;
        self.add_route("DELETE", path, handler, name, gil, py)
    }

    #[pyo3(signature = (method, path, handler, gil=false))]
    fn route(
        &mut self,
        method: &str,
        path: &str,
        handler: Py<PyAny>,
        gil: bool,
        py: Python<'_>,
    ) -> PyResult<()> {
        let name = handler.getattr(py, "__name__")?.extract::<String>(py)?;
        self.add_route(method, path, handler, name, gil, py)
    }

    fn before_request(&mut self, handler: Py<PyAny>, py: Python<'_>) -> PyResult<()> {
        let name = handler.getattr(py, "__name__")?.extract::<String>(py)?;
        let mut routes = self.routes.write();
        routes.before_hooks.push(handler);
        routes.before_hook_names.push(name);
        Ok(())
    }

    fn after_request(&mut self, handler: Py<PyAny>, py: Python<'_>) -> PyResult<()> {
        let name = handler.getattr(py, "__name__")?.extract::<String>(py)?;
        let mut routes = self.routes.write();
        routes.after_hooks.push(handler);
        routes.after_hook_names.push(name);
        Ok(())
    }

    fn fallback(&mut self, handler: Py<PyAny>, py: Python<'_>) -> PyResult<()> {
        let name = handler.getattr(py, "__name__")?.extract::<String>(py)?;
        let mut routes = self.routes.write();
        routes.fallback_handler = Some(handler);
        routes.fallback_handler_name = Some(name);
        Ok(())
    }

    fn websocket(&mut self, path: &str, handler: Py<PyAny>) -> PyResult<()> {
        let mut routes = self.routes.write();
        routes.ws_handlers.insert(path.to_string(), handler);
        Ok(())
    }

    fn static_dir(&mut self, prefix: &str, directory: &str) -> PyResult<()> {
        let prefix = if prefix.ends_with('/') {
            prefix.to_string()
        } else {
            format!("{prefix}/")
        };
        let dir = std::path::Path::new(directory)
            .canonicalize()
            .map_err(|e| {
                pyo3::exceptions::PyValueError::new_err(format!(
                    "static directory '{directory}' not found: {e}"
                ))
            })?
            .to_string_lossy()
            .to_string();
        let mut routes = self.routes.write();
        routes.static_dirs.push((prefix, dir));
        Ok(())
    }

    #[pyo3(signature = (host=None, port=None, workers=None, mode=None))]
    fn run(
        &self,
        py: Python<'_>,
        host: Option<&str>,
        port: Option<u16>,
        workers: Option<usize>,
        mode: Option<&str>,
    ) -> PyResult<()> {
        // Start GIL watchdog (once, opt-in via PYRE_METRICS=1)
        use std::sync::Once;
        static WATCHDOG_INIT: Once = Once::new();
        WATCHDOG_INIT.call_once(|| {
            if std::env::var("PYRE_METRICS").unwrap_or_default() == "1" {
                crate::monitor::spawn_gil_watchdog();
                println!("  GIL watchdog: enabled (PYRE_METRICS=1)");
            }
        });

        let host = host.unwrap_or("127.0.0.1");
        let port = port.unwrap_or(8000);
        let mode = mode.unwrap_or("default");
        let addr: SocketAddr = format!("{host}:{port}")
            .parse()
            .map_err(|e: std::net::AddrParseError| {
                pyo3::exceptions::PyValueError::new_err(e.to_string())
            })?;

        let num_cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let workers = workers.unwrap_or(num_cpus);

        // Freeze route table: extract from RwLock into read-only Arc.
        // After this point, no more route registration — zero-lock reads.
        let frozen: FrozenRoutes = {
            let table = self.routes.read();
            // Clone the RouteTable into a new Arc (no lock needed at runtime)
            Arc::new(RouteTable {
                handlers: table.handlers.iter().map(|h| h.clone_ref(py)).collect(),
                handler_names: table.handler_names.clone(),
                requires_gil: table.requires_gil.clone(),
                is_async: table.is_async.clone(),
                routers: table.routers.clone(),
                ws_handlers: table.ws_handlers.iter().map(|(k, v)| (k.clone(), v.clone_ref(py))).collect(),
                before_hooks: table.before_hooks.iter().map(|h| h.clone_ref(py)).collect(),
                after_hooks: table.after_hooks.iter().map(|h| h.clone_ref(py)).collect(),
                before_hook_names: table.before_hook_names.clone(),
                after_hook_names: table.after_hook_names.clone(),
                fallback_handler: table.fallback_handler.as_ref().map(|h| h.clone_ref(py)),
                fallback_handler_name: table.fallback_handler_name.clone(),
                static_dirs: table.static_dirs.clone(),
            })
        };

        if mode == "subinterp" || mode == "auto" {
            self.run_subinterp(py, addr, workers, num_cpus, frozen)
        } else {
            self.run_gil(py, addr, workers, num_cpus, frozen)
        }
    }
}

impl SkyApp {
    fn add_route(
        &mut self,
        method: &str,
        path: &str,
        handler: Py<PyAny>,
        handler_name: String,
        gil: bool,
        py: Python<'_>,
    ) -> PyResult<()> {
        // Auto-detect if handler is async def
        let inspect = py.import("inspect")?;
        let is_async = inspect
            .call_method1("iscoroutinefunction", (&handler,))?
            .extract::<bool>()?;

        let mut routes = self.routes.write();
        routes
            .insert(method, path, handler, handler_name, gil, is_async)
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("route error: {e}")))?;
        Ok(())
    }

    fn run_gil(
        &self,
        py: Python<'_>,
        addr: SocketAddr,
        workers: usize,
        num_cpus: usize,
        routes: FrozenRoutes,
    ) -> PyResult<()> {
        println!("\n  Pyre v1.0.0");
        println!("  Listening on http://{addr}");
        println!("  Workers: {workers} (CPUs: {num_cpus})\n");

        py.detach(move || -> PyResult<()> {
            let rt = RuntimeBuilder::new_multi_thread()
                .worker_threads(workers)
                .enable_all()
                .build()
                .map_err(|e| {
                    pyo3::exceptions::PyRuntimeError::new_err(format!("tokio runtime error: {e}"))
                })?;

            rt.block_on(async move {
                let listener = TcpListener::bind(addr).await.map_err(|e| {
                    pyo3::exceptions::PyOSError::new_err(format!("bind error: {e}"))
                })?;

                let shutdown = async {
                    signal::ctrl_c().await.ok();
                    println!("\n  Shutting down gracefully...");
                };
                tokio::pin!(shutdown);

                loop {
                    tokio::select! {
                        result = listener.accept() => {
                            let (stream, _) = result.map_err(|e| {
                                pyo3::exceptions::PyOSError::new_err(format!("accept error: {e}"))
                            })?;

                            let routes = Arc::clone(&routes);
                            let _ = stream.set_nodelay(true); // Disable Nagle for low latency
                            let io = TokioIo::new(stream);

                            tokio::spawn(async move {
                                let svc = service_fn(move |req: Request<Incoming>| {
                                    let routes = Arc::clone(&routes);
                                    async move {
                                        if websocket::is_websocket_upgrade(&req) {
                                            websocket::handle_websocket(req, routes).await
                                        } else {
                                            handle_request(req, routes).await
                                        }
                                    }
                                });

                                if let Err(e) = AutoBuilder::new(hyper_util::rt::TokioExecutor::new())
                                    .serve_connection_with_upgrades(io, svc)
                                    .await
                                {
                                    let msg = e.to_string();
                                    if !msg.contains("connection closed")
                                        && !msg.contains("reset by peer")
                                        && !msg.contains("broken pipe")
                                    {
                                        eprintln!("connection error: {e}");
                                    }
                                }
                            });
                        }
                        _ = &mut shutdown => {
                            break;
                        }
                    }
                }

                Ok(())
            })
        })
    }

    fn run_subinterp(
        &self,
        py: Python<'_>,
        addr: SocketAddr,
        workers: usize,
        num_cpus: usize,
        routes: FrozenRoutes,
    ) -> PyResult<()> {
        let script_path = if let Some(ref p) = self.script_path {
            p.clone()
        } else {
            let main_mod = py.import("__main__")?;
            main_mod.getattr("__file__")?.extract::<String>()?
        };

        let (handler_names, routers, before_hook_names, after_hook_names, static_dirs, requires_gil) = (
            routes.handler_names.clone(),
            routes.routers.clone(),
            routes.before_hook_names.clone(),
            routes.after_hook_names.clone(),
            routes.static_dirs.clone(),
            routes.requires_gil.clone(),
        );

        let gil_count = requires_gil.iter().filter(|&&g| g).count();
        let subinterp_count = requires_gil.len() - gil_count;

        let async_count_routes = routes.is_async.iter().filter(|&&a| a).count();
        let has_async = async_count_routes > 0;
        let mode_label = if has_async { "hybrid-async" } else { "hybrid" };
        println!("\n  Pyre v1.0.0 [{mode_label} mode]");
        println!("  Listening on http://{addr}");
        println!("  Sub-interpreters: {workers} (CPUs: {num_cpus})");
        if has_async {
            let sync_w = workers - (workers / 2).max(2);
            let async_w = (workers / 2).max(2);
            println!("  Workers: {sync_w} sync + {async_w} async");
        }
        println!("  Routes: {subinterp_count} sub-interp + {gil_count} GIL + {async_count_routes} async");
        println!("  Script: {script_path}\n");

        let pool = unsafe {
            interp::InterpreterPool::new(
                workers,
                py,
                &script_path,
                &handler_names,
                routers,
                &before_hook_names,
                &after_hook_names,
                static_dirs,
                requires_gil,
                routes.is_async.clone(),
            )
            .map_err(|e| {
                pyo3::exceptions::PyRuntimeError::new_err(format!(
                    "sub-interpreter pool error: {e}"
                ))
            })?
        };
        let pool = Arc::new(pool);

        py.detach(move || -> PyResult<()> {
            let rt = RuntimeBuilder::new_multi_thread()
                .worker_threads(workers)
                .enable_all()
                .build()
                .map_err(|e| {
                    pyo3::exceptions::PyRuntimeError::new_err(format!("tokio runtime error: {e}"))
                })?;

            rt.block_on(async move {
                let listener = TcpListener::bind(addr).await.map_err(|e| {
                    pyo3::exceptions::PyOSError::new_err(format!("bind error: {e}"))
                })?;

                let shutdown = async {
                    signal::ctrl_c().await.ok();
                    println!("\n  Shutting down gracefully...");
                };
                tokio::pin!(shutdown);

                loop {
                    tokio::select! {
                        result = listener.accept() => {
                            let (stream, _) = result.map_err(|e| {
                                pyo3::exceptions::PyOSError::new_err(format!("accept error: {e}"))
                            })?;

                            let pool = Arc::clone(&pool);
                            let routes = Arc::clone(&routes);
                            let _ = stream.set_nodelay(true); // Disable Nagle for low latency
                            let io = TokioIo::new(stream);

                            tokio::spawn(async move {
                                let svc = service_fn(move |req: Request<Incoming>| {
                                    let pool = Arc::clone(&pool);
                                    let routes = Arc::clone(&routes);
                                    async move {
                                        if websocket::is_websocket_upgrade(&req) {
                                            websocket::handle_websocket(req, routes).await
                                        } else {
                                            handle_request_subinterp(req, pool, routes).await
                                        }
                                    }
                                });

                                if let Err(e) = AutoBuilder::new(hyper_util::rt::TokioExecutor::new())
                                    .serve_connection_with_upgrades(io, svc)
                                    .await
                                {
                                    let msg = e.to_string();
                                    if !msg.contains("connection closed")
                                        && !msg.contains("reset by peer")
                                        && !msg.contains("broken pipe")
                                    {
                                        eprintln!("connection error: {e}");
                                    }
                                }
                            });
                        }
                        _ = &mut shutdown => {
                            break;
                        }
                    }
                }

                Ok(())
            })
        })
    }
}
