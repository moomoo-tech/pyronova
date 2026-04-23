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
pub(crate) fn setup_tcp_quickack(stream: &tokio::net::TcpStream) {
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
pub(crate) fn create_reuseport_listener(addr: SocketAddr) -> Result<std::net::TcpListener, String> {
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

    // TCP_DEFER_ACCEPT (Linux only): don't wake the accept loop on the
    // bare three-way handshake — wait until the client actually sends
    // the first byte of the HTTP request. A cold-connect flood
    // otherwise spins up Tokio tasks that immediately block in hyper's
    // header-read (or, if no data ever arrives, burn a file descriptor
    // until the header_read_timeout fires 10s later — see app.rs's
    // AutoBuilder config). Timeout arg is seconds after SYN-ACK before
    // the kernel gives up and delivers the bare accept anyway; keeping
    // it modest so half-open connections still surface within the
    // header-read budget.
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        let fd = socket.as_raw_fd();
        let secs: libc::c_int = 10;
        unsafe {
            libc::setsockopt(
                fd,
                libc::IPPROTO_TCP,
                libc::TCP_DEFER_ACCEPT,
                &secs as *const _ as *const libc::c_void,
                std::mem::size_of_val(&secs) as libc::socklen_t,
            );
        }
    }

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
pub(crate) async fn handle_accept_error(e: &std::io::Error) {
    let backoff_ms = match e.raw_os_error() {
        Some(libc::EMFILE) | Some(libc::ENFILE) | Some(libc::ENOBUFS) | Some(libc::ENOMEM) => {
            tracing::error!(
                target: "pyronova::server",
                error = %e,
                "accept() resource exhaustion — backing off 250ms",
            );
            250
        }
        _ => {
            tracing::warn!(target: "pyronova::server", error = %e, "accept() error");
            10
        }
    };
    tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
}

#[pyclass]
pub(crate) struct PyronovaApp {
    routes: MutableRoutes,
    script_path: Option<String>,
    shared_state: Arc<dashmap::DashMap<String, bytes::Bytes>>,
    /// Per-instance CORS configuration (None = disabled).
    cors_config: Option<crate::router::CorsConfig>,
    /// Per-instance request logging flag.
    request_logging: bool,
    /// Opt into Thread-Per-Core mode. See docs/tpc-rearch.md. Can also
    /// be flipped via the `PYRONOVA_TPC=1` env var; either is sufficient.
    tpc: bool,
}

#[pymethods]
impl PyronovaApp {
    #[new]
    fn new() -> Self {
        PyronovaApp {
            routes: Arc::new(parking_lot::RwLock::new(RouteTable::new())),
            script_path: None,
            shared_state: Arc::new(dashmap::DashMap::new()),
            cors_config: None,
            request_logging: false,
            tpc: false,
        }
    }

    /// Enable Thread-Per-Core mode (Phase 1 scaffolding).
    fn set_tpc(&mut self, enabled: bool) {
        self.tpc = enabled;
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
                target: "pyronova::server",
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

    /// Register a fast-path route — a response that never enters Python.
    ///
    /// For routes with a constant body (health checks, `/robots.txt`,
    /// `/pipeline` probe endpoints, maintenance pages) the Python handler
    /// dispatch is pure overhead: GIL acquisition, handler call,
    /// serialization, all for the same bytes every time. `add_fast_response`
    /// stores the fully-built response at registration time; the accept
    /// loop serves it directly without any Python involvement.
    ///
    /// The match is exact `(method, path)` — no path params, no glob.
    /// Path-parameterized routes still need a real handler.
    #[pyo3(signature = (
        method,
        path,
        body,
        content_type="text/plain".to_string(),
        status_code=200,
        headers=None
    ))]
    fn add_fast_response(
        &mut self,
        method: &str,
        path: &str,
        body: Vec<u8>,
        content_type: String,
        status_code: u16,
        headers: Option<std::collections::HashMap<String, String>>,
    ) -> PyResult<()> {
        let method_key = method.to_ascii_uppercase();
        let path_key = path.to_string();
        let resp = crate::router::FastResponse {
            body: bytes::Bytes::from(body),
            content_type,
            status: status_code,
            headers: headers.unwrap_or_default(),
        };
        self.routes
            .write()
            .fast_responses
            .entry(method_key)
            .or_default()
            .insert(path_key, resp);
        Ok(())
    }

    /// Configure response compression. Disabled by default; opt-in only.
    ///
    /// Args:
    ///   enabled: master switch — when false, compression logic is a
    ///     single relaxed-atomic load + branch-not-taken.
    ///   min_size: responses smaller than this (in bytes) are not compressed.
    ///   gzip / brotli: enable each algorithm. Server prefers brotli when both
    ///     are enabled and the client accepts it.
    ///   gzip_level: 1..=9, default 6. Higher = better ratio, more CPU.
    ///   brotli_quality: 0..=11, default 4. Production sweet spot is 4–6.
    #[pyo3(signature = (
        enabled,
        min_size = crate::compression::DEFAULT_MIN_SIZE,
        gzip = true,
        brotli = true,
        gzip_level = 6,
        brotli_quality = 4,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn configure_compression(
        &self,
        enabled: bool,
        min_size: usize,
        gzip: bool,
        brotli: bool,
        gzip_level: u32,
        brotli_quality: u32,
    ) {
        crate::compression::configure(enabled, min_size, gzip, brotli, gzip_level, brotli_quality);
    }

    /// Access the shared state (cross-sub-interpreter, nanosecond latency).
    #[getter]
    fn state(&self) -> SharedState {
        SharedState::with_inner(Arc::clone(&self.shared_state))
    }

    #[pyo3(signature = (path, handler, gil=false))]
    fn get(&mut self, path: &str, handler: Py<PyAny>, gil: bool, py: Python<'_>) -> PyResult<()> {
        let name = handler.getattr(py, "__name__")?.extract::<String>(py)?;
        self.add_route("GET", path, handler, name, gil, false, py)
    }

    #[pyo3(signature = (path, handler, gil=false, stream=false))]
    fn post(
        &mut self,
        path: &str,
        handler: Py<PyAny>,
        gil: bool,
        stream: bool,
        py: Python<'_>,
    ) -> PyResult<()> {
        let name = handler.getattr(py, "__name__")?.extract::<String>(py)?;
        self.add_route("POST", path, handler, name, gil, stream, py)
    }

    #[pyo3(signature = (path, handler, gil=false, stream=false))]
    fn put(
        &mut self,
        path: &str,
        handler: Py<PyAny>,
        gil: bool,
        stream: bool,
        py: Python<'_>,
    ) -> PyResult<()> {
        let name = handler.getattr(py, "__name__")?.extract::<String>(py)?;
        self.add_route("PUT", path, handler, name, gil, stream, py)
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
        self.add_route("DELETE", path, handler, name, gil, false, py)
    }

    #[pyo3(signature = (method, path, handler, gil=false, stream=false))]
    fn route(
        &mut self,
        method: &str,
        path: &str,
        handler: Py<PyAny>,
        gil: bool,
        stream: bool,
        py: Python<'_>,
    ) -> PyResult<()> {
        let name = handler.getattr(py, "__name__")?.extract::<String>(py)?;
        self.add_route(method, path, handler, name, gil, stream, py)
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

    #[pyo3(signature = (
        host=None, port=None, workers=None, mode=None, io_workers=None,
        tls_cert=None, tls_key=None, tpc=None,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn run(
        &self,
        py: Python<'_>,
        host: Option<&str>,
        port: Option<u16>,
        workers: Option<usize>,
        mode: Option<&str>,
        io_workers: Option<usize>,
        tls_cert: Option<&str>,
        tls_key: Option<&str>,
        tpc: Option<bool>,
    ) -> PyResult<()> {
        // Start RSS sampler (once, opt-in via PYRONOVA_METRICS=1).
        // GIL contention is now measured passively on the request path —
        // no active watchdog thread needed (zero observer-effect overhead).
        use std::sync::Once;
        static METRICS_INIT: Once = Once::new();
        METRICS_INIT.call_once(|| {
            crate::monitor::init_metrics_flag();
            if std::env::var("PYRONOVA_METRICS").unwrap_or_default() == "1" {
                crate::monitor::spawn_rss_sampler();
                tracing::info!(target: "pyronova::server", "Metrics enabled (PYRONOVA_METRICS=1): passive GIL monitor + RSS sampler");
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
        // io_workers = Tokio async thread count + accept loop count.
        // Default logic splits by mode (set further below once we know
        // whether TPC is active) — explicit override always wins.
        let io_workers_explicit = io_workers.is_some();
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
                is_stream: table.is_stream.clone(),
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
                fast_responses: table.fast_responses.clone(),
            })
        };

        // Build TLS acceptor once at startup if both paths are provided.
        // Either both or neither — single path is a configuration error.
        let tls_acceptor = match (tls_cert, tls_key) {
            (Some(cert), Some(key)) => Some(
                crate::tls::build_acceptor(cert, key)
                    .map_err(pyo3::exceptions::PyValueError::new_err)?,
            ),
            (None, None) => None,
            _ => {
                return Err(pyo3::exceptions::PyValueError::new_err(
                    "tls_cert and tls_key must be provided together",
                ))
            }
        };

        // TPC is now the default — automatic when the route set is
        // compatible (no gil=True / async def / stream=True routes).
        // Incompatible workloads fall back silently to the old
        // multi-thread InterpreterPool. Opt out explicitly via
        // `tpc=False`, `PYRONOVA_TPC=0`, or `PYRONOVA_TPC=off`.
        //
        // Why default-on: measured +7% throughput and ~10× P99
        // improvement over the multi_thread path on the baseline
        // test. Kernel SO_REUSEPORT + per-core current_thread runtime
        // + physical-core pinning + zero cross-thread handler dispatch
        // is a strict win for the common "sync handler" shape.
        // After Phase 3+4+5 TPC covers:
        //   gil=True     → main-interp bridge
        //   async def    → sub-interp asyncio loop (inline, blocking)
        //   stream=True  → main-interp bridge with stream response
        //                  (already requires gil=True per route registration)
        //
        // Nothing known to be TPC-incompatible as of v2.3 — we leave
        // the escape hatch `PYRONOVA_TPC=0` alive as a safety valve
        // for unforeseen bugs or niche C-extension loading issues.
        let tpc_incompatible = false;
        let tpc_forced_off = matches!(
            std::env::var("PYRONOVA_TPC").ok().as_deref(),
            Some("0") | Some("off") | Some("no") | Some("false")
        );
        let tpc_explicit_opt_in =
            tpc.unwrap_or(false) || self.tpc || crate::tpc::env_enabled();
        // Explicit opt-in on an incompatible route set is a startup
        // error (existing behavior in run_tpc_subinterp). Implicit
        // default on incompatible set silently falls back — this is
        // the whole point of the auto path.
        let tpc_enabled = if tpc_forced_off {
            false
        } else if tpc_explicit_opt_in {
            true // run_tpc_subinterp will error if incompatible
        } else {
            !tpc_incompatible
        };

        // TPC defaults to PHYSICAL core count, not logical. On SMT
        // systems, pinning 2 TPC threads to sibling hyperthreads
        // thrashes shared L1i/L1d and kills per-thread throughput
        // (measured -50% on 7840HS baseline). Work-stealing on
        // non-TPC can exploit SMT because one sibling does IO while
        // the other does Python bytecode — different cache footprints.
        // TPC runs the SAME codepath on every thread, so SMT siblings
        // step on each other. Explicit override via workers= or
        // PYRONOVA_IO_WORKERS env var still honored.
        let io_workers = if tpc_enabled && !io_workers_explicit {
            crate::tpc::physical_core_count()
        } else {
            io_workers
        };

        if mode == "subinterp" || mode == "auto" {
            if tpc_enabled {
                self.run_tpc_subinterp(
                    py,
                    addr,
                    workers,
                    io_workers,
                    num_cpus,
                    frozen,
                    tls_acceptor,
                )
            } else {
                self.run_subinterp(
                    py,
                    addr,
                    workers,
                    io_workers,
                    num_cpus,
                    frozen,
                    tls_acceptor,
                )
            }
        } else if tpc_enabled {
            self.run_tpc_gil(py, addr, io_workers, num_cpus, frozen, tls_acceptor)
        } else {
            self.run_gil(py, addr, io_workers, num_cpus, frozen, tls_acceptor)
        }
    }

    /// In-memory benchmark: spin up N TPC sub-interp workers, feed
    /// them virtual connections via `tokio::io::duplex` (no TCP).
    /// Bypasses the kernel network stack entirely — used to bound
    /// the pure-framework ceiling (Hyper parse → routing → handler
    /// → response build). Only supports sync, non-GIL, non-streaming
    /// routes. Returns `(total_requests, elapsed_s)`.
    #[pyo3(signature = (duration_s=10, workers=None, conns_per_worker=8))]
    fn bench_inmem(
        &self,
        py: Python<'_>,
        duration_s: u64,
        workers: Option<usize>,
        conns_per_worker: usize,
    ) -> PyResult<(u64, f64)> {
        self.__bench_inmem_impl(py, duration_s, workers, conns_per_worker)
    }

    /// Loopback bench: real TCP on 127.0.0.1, server + client in the
    /// same process. Measures the framework ceiling with the kernel
    /// network stack included, but zero external-client CPU
    /// contention (unlike wrk). Returns (total_requests, elapsed_s, port).
    #[pyo3(signature = (duration_s=10, workers=None, client_conns=32))]
    fn bench_loopback(
        &self,
        py: Python<'_>,
        duration_s: u64,
        workers: Option<usize>,
        client_conns: usize,
    ) -> PyResult<(u64, f64, u16)> {
        self.__bench_loopback_impl(py, duration_s, workers, client_conns)
    }
}

impl PyronovaApp {
    #[allow(clippy::too_many_arguments)]
    fn add_route(
        &mut self,
        method: &str,
        path: &str,
        handler: Py<PyAny>,
        handler_name: String,
        gil: bool,
        stream: bool,
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

        // Streaming constraints (v1): GIL-only, sync handlers only.
        // Sub-interp streaming needs a C-FFI bridge; async streaming needs
        // awaitable support. Both deferred to v2.
        if stream && !gil {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "stream=True requires gil=True (v1 limitation)",
            ));
        }
        if stream && is_async {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "stream=True is not yet supported on async def handlers (v1 limitation)",
            ));
        }

        let mut routes = self.routes.write();
        routes
            .insert(method, path, handler, handler_name, gil, is_async, stream)
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
        tls_acceptor: Option<Arc<tokio_rustls::TlsAcceptor>>,
    ) -> PyResult<()> {
        let scheme = if tls_acceptor.is_some() {
            "https"
        } else {
            "http"
        };
        tracing::info!(
            target: "pyronova::server",
            version = env!("CARGO_PKG_VERSION"),
            %addr,
            io_workers,
            cpus = num_cpus,
            mode = "gil",
            tls = tls_acceptor.is_some(),
            "Pyronova started"
        );
        println!("\n  Pyronova v{}", env!("CARGO_PKG_VERSION"));
        println!("  Listening on {scheme}://{addr}");
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
                    let tls_acc = tls_acceptor.clone();

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

                                    let conn_token = token.clone();
                                    let tls_acc_c = tls_acc.clone();
                                    tracker.spawn(async move {
                                        // TLS handshake happens here (inside the
                                        // spawned connection task) so it doesn't
                                        // block the accept loop from taking more
                                        // connections.
                                        let tls_stream = match tls_acc_c {
                                            Some(acc) => match crate::tls::wrap_tls(&acc, stream).await {
                                                Ok(s) => s,
                                                Err(e) => {
                                                    tracing::warn!(target: "pyronova::server", error = %e, "TLS handshake failed");
                                                    return;
                                                }
                                            },
                                            None => crate::tls::wrap_plain(stream),
                                        };
                                        let io = TokioIo::new(tls_stream);
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
                                        let mut builder = AutoBuilder::new(hyper_util::rt::TokioExecutor::new());
                                        // Slowloris defense: cap how long hyper waits for the
                                        // client to finish sending request headers. Without this
                                        // a client that opens a TCP connection and dribbles
                                        // one header byte per minute holds a Tokio task + fd
                                        // forever. TLS handshake is already bounded in
                                        // src/tls.rs::wrap_tls; this closes the analogous hole
                                        // on the plaintext HTTP path (and on HTTP-after-TLS).
                                        // HTTP/2 has its own internal frame/settings timeouts
                                        // via the h2 crate, so we only configure H/1 here.
                                        // Requires a Timer — TokioTimer ties it to the runtime.
                                        builder
                                            .http1()
                                            .timer(hyper_util::rt::TokioTimer::new())
                                            .header_read_timeout(std::time::Duration::from_secs(10));
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
                                                            tracing::warn!(target: "pyronova::server", error = %e, "Connection error");
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
                tracing::info!(target: "pyronova::server", "Shutting down gracefully...");
                println!("\n  Shutting down gracefully...");
                crate::monitor::stop_rss_sampler();
                shutdown_token.cancel();
                // Close the tracker (no more spawns) and wait for every
                // in-flight connection to finish its hyper drain. Bound
                // the wait at 30 s so a pathological client can't hold
                // shutdown hostage forever.
                conn_tracker.close();
                const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
                if tokio::time::timeout(DRAIN_TIMEOUT, conn_tracker.wait()).await.is_err() {
                    tracing::warn!(
                        target: "pyronova::server",
                        "{} in-flight connections did not drain within {:?} — exiting anyway",
                        conn_tracker.len(),
                        DRAIN_TIMEOUT,
                    );
                }

                Ok(())
            })
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn run_subinterp(
        &self,
        py: Python<'_>,
        addr: SocketAddr,
        workers: usize,
        io_workers: usize,
        num_cpus: usize,
        routes: FrozenRoutes,
        tls_acceptor: Option<Arc<tokio_rustls::TlsAcceptor>>,
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
            target: "pyronova::server",
            version = env!("CARGO_PKG_VERSION"),
            mode = mode_label,
            %addr,
            workers,
            cpus = num_cpus,
            subinterp_routes = subinterp_count,
            gil_routes = gil_count,
            async_routes = async_count_routes,
            "Pyronova started"
        );
        println!(
            "\n  Pyronova v{} [{mode_label} mode]",
            env!("CARGO_PKG_VERSION")
        );
        let scheme = if tls_acceptor.is_some() {
            "https"
        } else {
            "http"
        };
        println!("  Listening on {scheme}://{addr}");
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
                    let tls_acc = tls_acceptor.clone();

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

                                    let conn_token = token.clone();
                                    let tls_acc_c = tls_acc.clone();
                                    tracker.spawn(async move {
                                        let tls_stream = match tls_acc_c {
                                            Some(acc) => match crate::tls::wrap_tls(&acc, stream).await {
                                                Ok(s) => s,
                                                Err(e) => {
                                                    tracing::warn!(target: "pyronova::server", error = %e, "TLS handshake failed");
                                                    return;
                                                }
                                            },
                                            None => crate::tls::wrap_plain(stream),
                                        };
                                        let io = TokioIo::new(tls_stream);
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
                                        let mut builder = AutoBuilder::new(hyper_util::rt::TokioExecutor::new());
                                        // Slowloris defense: cap how long hyper waits for the
                                        // client to finish sending request headers. Without this
                                        // a client that opens a TCP connection and dribbles
                                        // one header byte per minute holds a Tokio task + fd
                                        // forever. TLS handshake is already bounded in
                                        // src/tls.rs::wrap_tls; this closes the analogous hole
                                        // on the plaintext HTTP path (and on HTTP-after-TLS).
                                        // HTTP/2 has its own internal frame/settings timeouts
                                        // via the h2 crate, so we only configure H/1 here.
                                        // Requires a Timer — TokioTimer ties it to the runtime.
                                        builder
                                            .http1()
                                            .timer(hyper_util::rt::TokioTimer::new())
                                            .header_read_timeout(std::time::Duration::from_secs(10));
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
                                                            tracing::warn!(target: "pyronova::server", error = %e, "Connection error");
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
                tracing::info!(target: "pyronova::server", "Shutting down gracefully...");
                println!("\n  Shutting down gracefully...");
                crate::monitor::stop_rss_sampler();
                shutdown_token.cancel();
                conn_tracker.close();
                const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
                if tokio::time::timeout(DRAIN_TIMEOUT, conn_tracker.wait()).await.is_err() {
                    tracing::warn!(
                        target: "pyronova::server",
                        "{} in-flight connections did not drain within {:?} — exiting anyway",
                        conn_tracker.len(),
                        DRAIN_TIMEOUT,
                    );
                }

                Ok(())
            })
        })
    }

    /// TPC GIL entry — Phase 1 scaffolding. Same dispatch semantics as
    /// `run_gil` (every handler on the main interpreter), different
    /// accept layer (N pinned OS threads × current_thread runtime ×
    /// SO_REUSEPORT, no cross-core task migration).
    #[allow(clippy::too_many_arguments)]
    fn run_tpc_gil(
        &self,
        py: Python<'_>,
        addr: SocketAddr,
        io_workers: usize,
        num_cpus: usize,
        routes: FrozenRoutes,
        tls_acceptor: Option<Arc<tokio_rustls::TlsAcceptor>>,
    ) -> PyResult<()> {
        py.detach(move || -> PyResult<()> {
            crate::tpc::run_tpc_gil(addr, io_workers, num_cpus, routes, tls_acceptor)
                .map_err(pyo3::exceptions::PyRuntimeError::new_err)
        })
    }

    /// TPC sub-interp inline mode (Phase 2) — each TPC thread owns its
    /// own sub-interp and runs handlers synchronously on the accept
    /// thread. No pool, no channel, no oneshot wake.
    ///
    /// Phase 2 constraint: every route must be sync + non-GIL. Any
    /// route with `gil=True`, `async def`, or `stream=True` causes this
    /// to bail at startup with a clear error. Users with such routes
    /// should stay on the old multi_thread path (drop `tpc=True`).
    #[allow(clippy::too_many_arguments)]
    fn run_tpc_subinterp(
        &self,
        py: Python<'_>,
        addr: SocketAddr,
        _workers: usize,
        io_workers: usize,
        num_cpus: usize,
        routes: FrozenRoutes,
        tls_acceptor: Option<Arc<tokio_rustls::TlsAcceptor>>,
    ) -> PyResult<()> {
        // gil=True routes: main-interp bridge (Phase 3).
        // async def: sub-interp path already drives coroutines via the
        //   persistent asyncio loop (SubInterpreterWorker::resolve_coroutine
        //   fires when call_handler returns an awaitable — line 1768 in
        //   interp.rs). Each sub-interp already has its own asyncio event
        //   loop cached at init. No extra work needed for correctness.
        //   Note: this is "blocking async" — the TPC thread is blocked
        //   for the coroutine's entire execution. Awaits inside the
        //   coroutine still run via asyncio's event loop on that same
        //   thread, so `await asyncio.sleep()` or `await client.get()`
        //   work correctly; they just don't yield to OTHER requests on
        //   the same TPC thread. Since SO_REUSEPORT distributes new
        //   connections across threads, this is fine for throughput —
        //   a slow async handler only blocks its one thread.
        // stream=True: already gated by gil=True in route registration,
        //   so all stream routes flow through the main-interp bridge.
        //   Phase 5 wires stream responses back through the bridge
        //   oneshot (BridgeResponse enum).
        let gil_count = routes.requires_gil.iter().filter(|&&g| g).count();
        let _async_count = routes.is_async.iter().filter(|&&a| a).count();
        let _stream_count = routes.is_stream.iter().filter(|&&s| s).count();

        let script_path = if let Some(ref p) = self.script_path {
            p.clone()
        } else {
            let main_mod = py.import("__main__")?;
            main_mod.getattr("__file__")?.extract::<String>()?
        };

        // Combine handler + hook names — same pattern as InterpreterPool::new.
        let mut all_func_names: Vec<String> = routes.handler_names.clone();
        all_func_names.extend(routes.before_hook_names.iter().cloned());
        all_func_names.extend(routes.after_hook_names.iter().cloned());

        // PYRONOVA_WORKER=1 so sub-interps know they're replays.
        std::env::set_var("PYRONOVA_WORKER", "1");

        // Read the user script once.
        let raw_script = std::fs::read_to_string(&script_path).map_err(|e| {
            pyo3::exceptions::PyRuntimeError::new_err(format!(
                "read script '{script_path}': {e}"
            ))
        })?;

        // Allocate a pool_id for this TPC server — still used by the
        // C-FFI bridge's zombie guard (cheap compatibility).
        let pool_id = interp::next_pool_id();

        // Build N sub-interpreters on the MAIN thread (main tstate current).
        // Each SubInterpreterWorker::new swaps to a fresh sub-interp, runs
        // the bootstrap script, then swaps back. Returned workers have
        // `tstate` saved (GIL released) — the TPC thread will rebind it
        // via rebind_tstate_to_current_thread before use.
        let n_threads = io_workers;
        let mut workers = Vec::with_capacity(n_threads);
        for i in 0..n_threads {
            let w = unsafe {
                interp::SubInterpreterWorker::new(&raw_script, &script_path, &all_func_names, pool_id)
                    .map_err(|e| {
                        pyo3::exceptions::PyRuntimeError::new_err(format!(
                            "TPC sub-interp {i} init: {e}"
                        ))
                    })?
            };
            workers.push(w);
        }

        // Phase 3 gil=True bridge: spawn a dedicated main-interp thread
        // when at least one route is gil=True. The bridge serves those
        // routes on a single MPSC-fed worker with the main GIL, while
        // TPC threads handle the rest inline. See src/main_bridge.rs.
        let main_bridge = if gil_count > 0 {
            let capacity: usize = std::env::var("PYRONOVA_GIL_BRIDGE_CAPACITY")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(16);
            Some(crate::main_bridge::MainInterpBridge::spawn(
                Arc::clone(&routes),
                capacity,
            ))
        } else {
            None
        };

        py.detach(move || -> PyResult<()> {
            crate::tpc::run_tpc_subinterp(
                addr,
                n_threads,
                num_cpus,
                workers,
                routes,
                tls_acceptor,
                main_bridge,
            )
            .map_err(pyo3::exceptions::PyRuntimeError::new_err)
        })
    }

    #[allow(dead_code)]
    fn __bench_inmem_impl(
        &self,
        py: Python<'_>,
        duration_s: u64,
        workers: Option<usize>,
        conns_per_worker: usize,
    ) -> PyResult<(u64, f64)> {
        let n_threads = workers.unwrap_or_else(crate::tpc::physical_core_count);

        // Build N independent FrozenRoutes — one per TPC worker. Each
        // gets its own Arc allocation so the refcount cacheline is
        // exclusive to the worker's P-core. Removes the cross-core
        // ping-pong from per-request Arc::clone(&routes) at the cost
        // of N × Py handler IncRefs at startup (one-time).
        let build_one = |py: Python<'_>| -> FrozenRoutes {
            let table = self.routes.read();
            Arc::new(RouteTable {
                handlers: table.handlers.iter().map(|h| h.clone_ref(py)).collect(),
                handler_names: table.handler_names.clone(),
                requires_gil: table.requires_gil.clone(),
                is_async: table.is_async.clone(),
                is_stream: table.is_stream.clone(),
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
                fast_responses: table.fast_responses.clone(),
            })
        };

        // Route-shape validation uses one sample.
        let sample = build_one(py);
        if sample.requires_gil.iter().any(|&g| g) {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "bench_inmem does not support gil=True routes",
            ));
        }
        if sample.is_async.iter().any(|&a| a) {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "bench_inmem does not support async def routes",
            ));
        }
        if sample.is_stream.iter().any(|&s| s) {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "bench_inmem does not support stream=True routes",
            ));
        }
        let handler_names_from_routes = sample.handler_names.clone();
        let before_hook_names = sample.before_hook_names.clone();
        let after_hook_names = sample.after_hook_names.clone();

        let mut per_worker_routes: Vec<FrozenRoutes> = Vec::with_capacity(n_threads);
        per_worker_routes.push(sample);
        for _ in 1..n_threads {
            per_worker_routes.push(build_one(py));
        }

        let script_path = if let Some(ref p) = self.script_path {
            p.clone()
        } else {
            let main_mod = py.import("__main__")?;
            main_mod.getattr("__file__")?.extract::<String>()?
        };
        let mut all_func_names: Vec<String> = handler_names_from_routes;
        all_func_names.extend(before_hook_names.into_iter());
        all_func_names.extend(after_hook_names.into_iter());

        crate::monitor::init_metrics_flag();
        std::env::set_var("PYRONOVA_WORKER", "1");
        let raw_script = std::fs::read_to_string(&script_path).map_err(|e| {
            pyo3::exceptions::PyRuntimeError::new_err(format!(
                "read script '{script_path}': {e}"
            ))
        })?;
        let pool_id = interp::next_pool_id();
        let mut built_workers = Vec::with_capacity(n_threads);
        for i in 0..n_threads {
            let w = unsafe {
                interp::SubInterpreterWorker::new(
                    &raw_script,
                    &script_path,
                    &all_func_names,
                    pool_id,
                )
                .map_err(|e| {
                    pyo3::exceptions::PyRuntimeError::new_err(format!(
                        "bench_inmem sub-interp {i} init: {e}"
                    ))
                })?
            };
            built_workers.push(w);
        }

        py.detach(move || -> PyResult<(u64, f64)> {
            crate::bench::run_inmem_bench(
                n_threads,
                conns_per_worker,
                duration_s,
                built_workers,
                per_worker_routes,
                None,
            )
            .map_err(pyo3::exceptions::PyRuntimeError::new_err)
        })
    }

    #[allow(dead_code)]
    fn __bench_loopback_impl(
        &self,
        py: Python<'_>,
        duration_s: u64,
        workers: Option<usize>,
        client_conns: usize,
    ) -> PyResult<(u64, f64, u16)> {
        let routes: FrozenRoutes = {
            let table = self.routes.read();
            Arc::new(RouteTable {
                handlers: table.handlers.iter().map(|h| h.clone_ref(py)).collect(),
                handler_names: table.handler_names.clone(),
                requires_gil: table.requires_gil.clone(),
                is_async: table.is_async.clone(),
                is_stream: table.is_stream.clone(),
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
                fast_responses: table.fast_responses.clone(),
            })
        };

        if routes.requires_gil.iter().any(|&g| g)
            || routes.is_async.iter().any(|&a| a)
            || routes.is_stream.iter().any(|&s| s)
        {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "bench_loopback requires all routes to be sync, non-GIL, non-stream",
            ));
        }

        let n_threads = workers.unwrap_or_else(crate::tpc::physical_core_count);
        let script_path = if let Some(ref p) = self.script_path {
            p.clone()
        } else {
            let main_mod = py.import("__main__")?;
            main_mod.getattr("__file__")?.extract::<String>()?
        };
        let mut all_func_names: Vec<String> = routes.handler_names.clone();
        all_func_names.extend(routes.before_hook_names.iter().cloned());
        all_func_names.extend(routes.after_hook_names.iter().cloned());

        crate::monitor::init_metrics_flag();
        std::env::set_var("PYRONOVA_WORKER", "1");
        let raw_script = std::fs::read_to_string(&script_path).map_err(|e| {
            pyo3::exceptions::PyRuntimeError::new_err(format!(
                "read script '{script_path}': {e}"
            ))
        })?;
        let pool_id = interp::next_pool_id();
        let mut built_workers = Vec::with_capacity(n_threads);
        for i in 0..n_threads {
            let w = unsafe {
                interp::SubInterpreterWorker::new(
                    &raw_script,
                    &script_path,
                    &all_func_names,
                    pool_id,
                )
                .map_err(|e| {
                    pyo3::exceptions::PyRuntimeError::new_err(format!(
                        "bench_loopback sub-interp {i} init: {e}"
                    ))
                })?
            };
            built_workers.push(w);
        }

        py.detach(move || -> PyResult<(u64, f64, u16)> {
            crate::bench::run_loopback_bench(
                n_threads,
                client_conns,
                duration_s,
                built_workers,
                routes,
                None,
            )
            .map_err(pyo3::exceptions::PyRuntimeError::new_err)
        })
    }
}
