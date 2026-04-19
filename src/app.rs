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

/// Enable TCP_QUICKACK on a stream (Linux only, no-op elsewhere).
#[allow(unused_variables)]
fn setup_tcp_quickack(stream: &tokio::net::TcpStream) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        let fd = stream.as_raw_fd();
        let val: libc::c_int = 1;
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_TCP,
                libc::TCP_QUICKACK,
                &val as *const _ as *const libc::c_void,
                std::mem::size_of_val(&val) as libc::socklen_t,
            );
        }
    }
}

/// Create a TCP listener with SO_REUSEPORT (kernel load-balanced accept)
/// and a large backlog (8192) to avoid SYN drops under extreme load.
fn create_reuseport_listener(addr: SocketAddr) -> Result<std::net::TcpListener, String> {
    use socket2::{Domain, Protocol, Socket, Type};

    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))
        .map_err(|e| format!("socket creation error: {e}"))?;

    socket
        .set_reuse_address(true)
        .map_err(|e| format!("set_reuse_address error: {e}"))?;

    // SO_REUSEPORT: allows multiple listeners on the same port.
    // Kernel distributes incoming connections across all listeners.
    #[cfg(not(windows))]
    socket
        .set_reuse_port(true)
        .map_err(|e| format!("set_reuse_port error: {e}"))?;

    socket
        .set_nonblocking(true)
        .map_err(|e| format!("set_nonblocking error: {e}"))?;

    socket
        .bind(&addr.into())
        .map_err(|e| format!("bind error: {e}"))?;

    // Large backlog to avoid SYN drops at 200k+ QPS.
    socket
        .listen(8192)
        .map_err(|e| format!("listen error: {e}"))?;

    Ok(socket.into())
}

use crate::handlers::{handle_request, handle_request_subinterp};
use crate::interp;
use crate::router::{FrozenRoutes, MutableRoutes, RouteTable};
use crate::state::SharedState;
use crate::websocket;

/// Back off when accept() fails. Critical for EMFILE/ENFILE (file-descriptor
/// exhaustion) — a bare `continue` on these errors spins the accept loop at
/// 100% CPU because the next accept() call fails immediately. Sleeping a few
/// hundred ms lets short-lived fds close and gives the OS room to recover.
/// Transient per-connection errors (ECONNABORTED etc.) get a tiny yield to
/// avoid degenerate tight loops without meaningfully delaying legitimate traffic.
async fn handle_accept_error(e: &std::io::Error) {
    let backoff_ms = match e.raw_os_error() {
        Some(libc::EMFILE) | Some(libc::ENFILE) | Some(libc::ENOBUFS) | Some(libc::ENOMEM) => {
            tracing::error!(
                target: "pyre::server",
                error = %e,
                "accept() resource exhaustion — backing off 250ms",
            );
            250
        }
        _ => {
            tracing::warn!(target: "pyre::server", error = %e, "accept() error");
            10
        }
    };
    tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
}

#[pyclass]
pub(crate) struct PyreApp {
    routes: MutableRoutes,
    script_path: Option<String>,
    shared_state: Arc<dashmap::DashMap<String, bytes::Bytes>>,
    /// Per-instance CORS configuration (None = disabled).
    cors_config: Option<crate::router::CorsConfig>,
    /// Per-instance request logging flag.
    request_logging: bool,
}

#[pymethods]
impl PyreApp {
    #[new]
    fn new() -> Self {
        PyreApp {
            routes: Arc::new(parking_lot::RwLock::new(RouteTable::new())),
            script_path: None,
            shared_state: Arc::new(dashmap::DashMap::new()),
            cors_config: None,
            request_logging: false,
        }
    }

    /// Set per-instance CORS origin (legacy setter — disables advanced CORS
    /// features. Prefer `set_cors_config` which propagates credentials and
    /// expose-headers to every response per W3C CORS spec.)
    fn set_cors_origin(&mut self, origin: String) {
        self.cors_config = Some(crate::router::CorsConfig {
            origin,
            methods: "GET, POST, PUT, DELETE, PATCH, OPTIONS".to_string(),
            headers: "*".to_string(),
            expose_headers: None,
            allow_credentials: false,
        });
    }

    /// Set full per-instance CORS configuration. All fields are applied to
    /// every response (GET/POST/etc.), not just OPTIONS preflight.
    #[pyo3(signature = (origin, methods, headers, expose_headers=None, allow_credentials=false))]
    fn set_cors_config(
        &mut self,
        origin: String,
        methods: String,
        headers: String,
        expose_headers: Option<String>,
        allow_credentials: bool,
    ) {
        // W3C Fetch / CORS forbids `Access-Control-Allow-Origin: *`
        // together with `Access-Control-Allow-Credentials: true` —
        // browsers reject the response client-side regardless of
        // what the server sends. The server still returns 200 which
        // makes this a particularly nasty debugging pit (200 logs,
        // client-visible failure). Warn at config time so the
        // misconfiguration is visible in the logs where the user
        // looks first.
        if allow_credentials && origin.trim() == "*" {
            tracing::warn!(
                target: "pyre::server",
                "CORS misconfiguration: origin=\"*\" with allow_credentials=true is rejected by all \
                 major browsers (W3C Fetch spec). Configure a concrete origin (e.g. \"https://app.example.com\") \
                 when credentials are enabled."
            );
        }
        self.cors_config = Some(crate::router::CorsConfig {
            origin,
            methods,
            headers,
            expose_headers: expose_headers.filter(|s| !s.is_empty()),
            allow_credentials,
        });
    }

    /// Enable/disable per-instance request logging.
    fn enable_request_logging(&mut self, enabled: bool) {
        self.request_logging = enabled;
    }

    /// Set max request body size in bytes. Default: 10 MB.
    fn set_max_body_size(&self, size: usize) {
        crate::handlers::set_max_body_size(size);
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
    fn delete(
        &mut self,
        path: &str,
        handler: Py<PyAny>,
        gil: bool,
        py: Python<'_>,
    ) -> PyResult<()> {
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

    #[pyo3(signature = (host=None, port=None, workers=None, mode=None, io_workers=None))]
    fn run(
        &self,
        py: Python<'_>,
        host: Option<&str>,
        port: Option<u16>,
        workers: Option<usize>,
        mode: Option<&str>,
        io_workers: Option<usize>,
    ) -> PyResult<()> {
        // Start RSS sampler (once, opt-in via PYRE_METRICS=1).
        // GIL contention is now measured passively on the request path —
        // no active watchdog thread needed (zero observer-effect overhead).
        use std::sync::Once;
        static METRICS_INIT: Once = Once::new();
        METRICS_INIT.call_once(|| {
            if std::env::var("PYRE_METRICS").unwrap_or_default() == "1" {
                crate::monitor::spawn_rss_sampler();
                tracing::info!(target: "pyre::server", "Metrics enabled (PYRE_METRICS=1): passive GIL monitor + RSS sampler");
            }
        });

        let host = host.unwrap_or("127.0.0.1");
        let port = port.unwrap_or(8000);
        let mode = mode.unwrap_or("default");
        let addr: SocketAddr =
            format!("{host}:{port}")
                .parse()
                .map_err(|e: std::net::AddrParseError| {
                    pyo3::exceptions::PyValueError::new_err(e.to_string())
                })?;

        let num_cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        // workers = Python sub-interpreter count (default: num_cpus)
        let workers = workers.unwrap_or(num_cpus);
        // io_workers = Tokio async thread count + accept loop count (default: num_cpus)
        let io_workers = io_workers.unwrap_or(num_cpus);

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
                ws_handlers: table
                    .ws_handlers
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone_ref(py)))
                    .collect(),
                before_hooks: table.before_hooks.iter().map(|h| h.clone_ref(py)).collect(),
                after_hooks: table.after_hooks.iter().map(|h| h.clone_ref(py)).collect(),
                before_hook_names: table.before_hook_names.clone(),
                after_hook_names: table.after_hook_names.clone(),
                fallback_handler: table.fallback_handler.as_ref().map(|h| h.clone_ref(py)),
                fallback_handler_name: table.fallback_handler_name.clone(),
                static_dirs: table.static_dirs.clone(),
                cors_config: self.cors_config.clone(),
                request_logging: self.request_logging,
            })
        };

        if mode == "subinterp" || mode == "auto" {
            self.run_subinterp(py, addr, workers, io_workers, num_cpus, frozen)
        } else {
            self.run_gil(py, addr, io_workers, num_cpus, frozen)
        }
    }
}

impl PyreApp {
    fn add_route(
        &mut self,
        method: &str,
        path: &str,
        handler: Py<PyAny>,
        handler_name: String,
        gil: bool,
        py: Python<'_>,
    ) -> PyResult<()> {
        // Auto-detect if handler is async def (also check __call__ for class-based views)
        let inspect = py.import("inspect")?;
        let is_async = inspect
            .call_method1("iscoroutinefunction", (&handler,))?
            .extract::<bool>()
            .unwrap_or(false)
            || handler
                .bind(py)
                .getattr("__call__")
                .and_then(|c| {
                    inspect
                        .call_method1("iscoroutinefunction", (c,))
                        .and_then(|r| r.extract::<bool>())
                })
                .unwrap_or(false);

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
        io_workers: usize,
        num_cpus: usize,
        routes: FrozenRoutes,
    ) -> PyResult<()> {
        tracing::info!(
            target: "pyre::server",
            version = env!("CARGO_PKG_VERSION"),
            %addr,
            io_workers,
            cpus = num_cpus,
            mode = "gil",
            "Pyre started"
        );
        println!("\n  Pyre v{}", env!("CARGO_PKG_VERSION"));
        println!("  Listening on http://{addr}");
        println!("  IO workers: {io_workers} (CPUs: {num_cpus})\n");

        py.detach(move || -> PyResult<()> {
            let rt = RuntimeBuilder::new_multi_thread()
                .worker_threads(io_workers)
                .enable_all()
                .build()
                .map_err(|e| {
                    pyo3::exceptions::PyRuntimeError::new_err(format!("tokio runtime error: {e}"))
                })?;

            rt.block_on(async move {
                // Multi-accept: N listeners on same port via SO_REUSEPORT.
                // Linux kernel load-balances connections across accept loops.
                // macOS SO_REUSEPORT doesn't do kernel LB, so use 1 acceptor.
                #[cfg(target_os = "linux")]
                let n_accept = io_workers.min(num_cpus);
                #[cfg(not(target_os = "linux"))]
                let n_accept = 1;
                let shutdown_token = tokio_util::sync::CancellationToken::new();
                // TaskTracker: collects every per-connection spawn so we can
                // `.wait()` for them on shutdown. Without this, `rt.block_on`
                // returning after `shutdown_token.cancel()` drops the Tokio
                // Runtime, which aborts every spawned connection mid-request
                // (clients see TCP RST). graceful_shutdown() on each conn is
                // necessary but insufficient — it only signals hyper to stop
                // accepting NEW keep-alive requests; the drain still needs
                // time on the runtime.
                let conn_tracker = tokio_util::task::TaskTracker::new();

                for _ in 0..n_accept {
                    let std_listener = create_reuseport_listener(addr).map_err(|e| {
                        pyo3::exceptions::PyOSError::new_err(e)
                    })?;
                    let listener = TcpListener::from_std(std_listener).map_err(|e| {
                        pyo3::exceptions::PyOSError::new_err(format!("TcpListener::from_std error: {e}"))
                    })?;
                    let routes = Arc::clone(&routes);
                    let token = shutdown_token.clone();
                    let tracker = conn_tracker.clone();

                    tokio::spawn(async move {
                        loop {
                            tokio::select! {
                                result = listener.accept() => {
                                    let (stream, remote_addr) = match result {
                                        Ok(v) => v,
                                        Err(e) => {
                                            handle_accept_error(&e).await;
                                            continue;
                                        }
                                    };
                                    let routes = Arc::clone(&routes);
                                    let _ = stream.set_nodelay(true);
                                    setup_tcp_quickack(&stream);
                                    let io = TokioIo::new(stream);

                                    let conn_token = token.clone();
                                    tracker.spawn(async move {
                                        let svc = service_fn(move |req: Request<Incoming>| {
                                            let routes = Arc::clone(&routes);
                                            let client_ip_addr = remote_addr.ip();
                                            async move {
                                                if websocket::is_websocket_upgrade(&req) {
                                                    websocket::handle_websocket(req, routes).await
                                                } else {
                                                    handle_request(req, routes, client_ip_addr).await
                                                }
                                            }
                                        });
                                        let builder = AutoBuilder::new(hyper_util::rt::TokioExecutor::new());
                                        let conn = builder.serve_connection_with_upgrades(io, svc);
                                        tokio::pin!(conn);
                                        let mut graceful_sent = false;
                                        loop {
                                            tokio::select! {
                                                res = conn.as_mut() => {
                                                    if let Err(e) = res {
                                                        let msg = e.to_string();
                                                        if !msg.contains("connection closed")
                                                            && !msg.contains("reset by peer")
                                                            && !msg.contains("broken pipe")
                                                        {
                                                            tracing::warn!(target: "pyre::server", error = %e, "Connection error");
                                                        }
                                                    }
                                                    break;
                                                }
                                                _ = conn_token.cancelled(), if !graceful_sent => {
                                                    // Shutdown: tell hyper to stop accepting
                                                    // new requests on this connection and drain
                                                    // in-flight ones. Keep driving the
                                                    // connection future until it completes.
                                                    conn.as_mut().graceful_shutdown();
                                                    graceful_sent = true;
                                                }
                                            }
                                        }
                                    });
                                }
                                _ = token.cancelled() => break,
                            }
                        }
                    });
                }

                signal::ctrl_c().await.ok();
                tracing::info!(target: "pyre::server", "Shutting down gracefully...");
                println!("\n  Shutting down gracefully...");
                crate::monitor::stop_rss_sampler();
                shutdown_token.cancel();
                // Close the tracker (no more spawns) and wait for every
                // in-flight connection to finish its hyper drain. Bound
                // the wait at 30 s so a pathological client can't hold
                // shutdown hostage forever.
                conn_tracker.close();
                const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
                if let Err(_) = tokio::time::timeout(DRAIN_TIMEOUT, conn_tracker.wait()).await {
                    tracing::warn!(
                        target: "pyre::server",
                        "{} in-flight connections did not drain within {:?} — exiting anyway",
                        conn_tracker.len(),
                        DRAIN_TIMEOUT,
                    );
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
        io_workers: usize,
        num_cpus: usize,
        routes: FrozenRoutes,
    ) -> PyResult<()> {
        let script_path = if let Some(ref p) = self.script_path {
            p.clone()
        } else {
            let main_mod = py.import("__main__")?;
            main_mod.getattr("__file__")?.extract::<String>()?
        };

        let (
            handler_names,
            routers,
            before_hook_names,
            after_hook_names,
            static_dirs,
            requires_gil,
        ) = (
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
        tracing::info!(
            target: "pyre::server",
            version = env!("CARGO_PKG_VERSION"),
            mode = mode_label,
            %addr,
            workers,
            cpus = num_cpus,
            subinterp_routes = subinterp_count,
            gil_routes = gil_count,
            async_routes = async_count_routes,
            "Pyre started"
        );
        println!(
            "\n  Pyre v{} [{mode_label} mode]",
            env!("CARGO_PKG_VERSION")
        );
        println!("  Listening on http://{addr}");
        println!("  Sub-interpreters: {workers} | IO threads: {io_workers} (CPUs: {num_cpus})");
        if has_async {
            let sync_w = workers - (workers / 2).max(2);
            let async_w = (workers / 2).max(2);
            println!("  Workers: {sync_w} sync + {async_w} async");
        }
        println!(
            "  Routes: {subinterp_count} sub-interp + {gil_count} GIL + {async_count_routes} async"
        );
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
                self.cors_config.clone(),
                self.request_logging,
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
                .worker_threads(io_workers)
                .enable_all()
                .build()
                .map_err(|e| {
                    pyo3::exceptions::PyRuntimeError::new_err(format!("tokio runtime error: {e}"))
                })?;

            rt.block_on(async move {
                #[cfg(target_os = "linux")]
                let n_accept = io_workers.min(num_cpus);
                #[cfg(not(target_os = "linux"))]
                let n_accept = 1;
                let shutdown_token = tokio_util::sync::CancellationToken::new();
                // See comment in run_gil — same contract.
                let conn_tracker = tokio_util::task::TaskTracker::new();

                for _ in 0..n_accept {
                    let std_listener = create_reuseport_listener(addr).map_err(|e| {
                        pyo3::exceptions::PyOSError::new_err(e)
                    })?;
                    let listener = TcpListener::from_std(std_listener).map_err(|e| {
                        pyo3::exceptions::PyOSError::new_err(format!("TcpListener::from_std error: {e}"))
                    })?;
                    let pool = Arc::clone(&pool);
                    let routes = Arc::clone(&routes);
                    let token = shutdown_token.clone();
                    let tracker = conn_tracker.clone();

                    tokio::spawn(async move {
                        loop {
                            tokio::select! {
                                result = listener.accept() => {
                                    let (stream, remote_addr) = match result {
                                        Ok(v) => v,
                                        Err(e) => {
                                            handle_accept_error(&e).await;
                                            continue;
                                        }
                                    };
                                    let pool = Arc::clone(&pool);
                                    let routes = Arc::clone(&routes);
                                    let _ = stream.set_nodelay(true);
                                    setup_tcp_quickack(&stream);
                                    let io = TokioIo::new(stream);

                                    let conn_token = token.clone();
                                    tracker.spawn(async move {
                                        let svc = service_fn(move |req: Request<Incoming>| {
                                            let pool = Arc::clone(&pool);
                                            let routes = Arc::clone(&routes);
                                            let client_ip_addr = remote_addr.ip();
                                            async move {
                                                if websocket::is_websocket_upgrade(&req) {
                                                    websocket::handle_websocket(req, routes).await
                                                } else {
                                                    handle_request_subinterp(req, pool, routes, client_ip_addr).await
                                                }
                                            }
                                        });
                                        let builder = AutoBuilder::new(hyper_util::rt::TokioExecutor::new());
                                        let conn = builder.serve_connection_with_upgrades(io, svc);
                                        tokio::pin!(conn);
                                        let mut graceful_sent = false;
                                        loop {
                                            tokio::select! {
                                                res = conn.as_mut() => {
                                                    if let Err(e) = res {
                                                        let msg = e.to_string();
                                                        if !msg.contains("connection closed")
                                                            && !msg.contains("reset by peer")
                                                            && !msg.contains("broken pipe")
                                                        {
                                                            tracing::warn!(target: "pyre::server", error = %e, "Connection error");
                                                        }
                                                    }
                                                    break;
                                                }
                                                _ = conn_token.cancelled(), if !graceful_sent => {
                                                    conn.as_mut().graceful_shutdown();
                                                    graceful_sent = true;
                                                }
                                            }
                                        }
                                    });
                                }
                                _ = token.cancelled() => break,
                            }
                        }
                    });
                }

                signal::ctrl_c().await.ok();
                tracing::info!(target: "pyre::server", "Shutting down gracefully...");
                println!("\n  Shutting down gracefully...");
                crate::monitor::stop_rss_sampler();
                shutdown_token.cancel();
                conn_tracker.close();
                const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
                if let Err(_) = tokio::time::timeout(DRAIN_TIMEOUT, conn_tracker.wait()).await {
                    tracing::warn!(
                        target: "pyre::server",
                        "{} in-flight connections did not drain within {:?} — exiting anyway",
                        conn_tracker.len(),
                        DRAIN_TIMEOUT,
                    );
                }

                Ok(())
            })
        })
    }
}
