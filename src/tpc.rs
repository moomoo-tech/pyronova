//! Thread-Per-Core (TPC) accept layer. See docs/tpc-rearch.md.
//!
//! Phase 1: scaffolding only. Each TPC thread owns a pinned OS thread, a
//! `tokio::runtime::Builder::new_current_thread()` runtime, a
//! `LocalSet` for spawn_local tasks, and its own `SO_REUSEPORT`
//! listener. Dispatch still flows through the old `InterpreterPool` —
//! Phase 2 replaces that with a per-thread sub-interpreter.
//!
//! Opt-in via `PYRONOVA_TPC=1` (env) or `app.run(tpc=True)` (kwarg).
//! Old multi-thread path remains the default until Phase 1 proves out.
//!
//! Why no `Send` bounds on the per-connection future? Because
//! `LocalSet::spawn_local` runs the task on the same OS thread that
//! owns the LocalSet — no cross-thread move ever happens. This is also
//! why we don't pay work-stealing cost on this path: there is no other
//! worker to steal from.

use std::net::SocketAddr;
use std::sync::Arc;

use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::Request;
use hyper_util::rt::TokioIo;
use hyper_util::server::conn::auto::Builder as AutoBuilder;
use tokio::net::TcpListener;
use tokio::runtime::Builder as RuntimeBuilder;
use tokio::task::LocalSet;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::app::{create_reuseport_listener, handle_accept_error, setup_tcp_quickack};
use crate::handlers::{handle_request, handle_request_tpc_inline};
use crate::interp::SubInterpreterWorker;
use crate::router::FrozenRoutes;
use crate::websocket;

/// Custom hyper executor that spawns onto the current thread's
/// `LocalSet`. Required because `hyper_util::rt::TokioExecutor` uses
/// `tokio::spawn` which needs a multi-thread runtime — on a
/// current-thread runtime that call panics. `LocalExec` drops the Send
/// bound on the future, keeping every spawn strictly on the TPC
/// thread.
#[derive(Clone, Copy)]
struct LocalExec;

impl<F> hyper::rt::Executor<F> for LocalExec
where
    F: std::future::Future + 'static,
    F::Output: 'static,
{
    fn execute(&self, fut: F) {
        tokio::task::spawn_local(fut);
    }
}

/// Pin the current OS thread to a specific CPU core if one is
/// available. Silently no-ops on platforms where `core_affinity`
/// can't enumerate (e.g. restricted containers with no CPU mask
/// visibility); in that case the OS scheduler still gets us
/// statistically-close-to-core-local execution on the per-thread
/// runtime because the runtime never migrates tasks, only the
/// kernel can move the thread.
fn try_pin_current(core_id: Option<core_affinity::CoreId>) {
    if let Some(c) = core_id {
        let _ = core_affinity::set_for_current(c);
    }
}

/// Log line emitted once on startup so operators can see the TPC topology.
fn log_startup(mode: &str, addr: &SocketAddr, n_threads: usize, n_cpus: usize) {
    tracing::info!(
        target: "pyronova::server",
        version = env!("CARGO_PKG_VERSION"),
        mode,
        tpc = true,
        %addr,
        tpc_threads = n_threads,
        cpus = n_cpus,
        "Pyronova TPC started"
    );
    println!(
        "\n  Pyronova v{} [TPC mode, {mode}]",
        env!("CARGO_PKG_VERSION")
    );
    println!("  Listening on http://{addr}");
    println!("  TPC threads: {n_threads} (CPUs: {n_cpus}, pinned)\n");
}

// ---------------------------------------------------------------------------
// GIL mode — every handler on the main interpreter
// ---------------------------------------------------------------------------

pub(crate) fn run_tpc_gil(
    addr: SocketAddr,
    n_threads: usize,
    n_cpus: usize,
    routes: FrozenRoutes,
    tls_acceptor: Option<Arc<tokio_rustls::TlsAcceptor>>,
) -> Result<(), String> {
    log_startup("gil", &addr, n_threads, n_cpus);

    let shutdown = CancellationToken::new();
    let core_ids = core_affinity::get_core_ids().unwrap_or_default();

    // ctrl_c watcher runs on its own dedicated thread — it's not TPC
    // traffic, just a signal sink that flips the token.
    let sigint_token = shutdown.clone();
    std::thread::spawn(move || {
        let rt = RuntimeBuilder::new_current_thread()
            .enable_all()
            .build()
            .expect("sigint runtime");
        rt.block_on(async move {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!(target: "pyronova::server", "Shutting down gracefully...");
            println!("\n  Shutting down gracefully...");
            crate::monitor::stop_rss_sampler();
            sigint_token.cancel();
        });
    });

    let mut handles = Vec::with_capacity(n_threads);
    for i in 0..n_threads {
        let core_id = core_ids.get(i).copied();
        let routes = Arc::clone(&routes);
        let shutdown = shutdown.clone();
        let tls = tls_acceptor.clone();

        let handle = std::thread::Builder::new()
            .name(format!("pyronova-tpc-{i}"))
            .spawn(move || {
                try_pin_current(core_id);
                let rt = RuntimeBuilder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tpc current-thread runtime build");
                let local = LocalSet::new();
                local.block_on(&rt, async move {
                    tpc_accept_loop_gil(addr, routes, shutdown, tls).await;
                });
            })
            .map_err(|e| format!("spawn tpc-{i}: {e}"))?;
        handles.push(handle);
    }

    for h in handles {
        let _ = h.join();
    }
    Ok(())
}

async fn tpc_accept_loop_gil(
    addr: SocketAddr,
    routes: FrozenRoutes,
    shutdown: CancellationToken,
    tls_acceptor: Option<Arc<tokio_rustls::TlsAcceptor>>,
) {
    let std_listener = match create_reuseport_listener(addr) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(target: "pyronova::server", error = %e, "TPC reuseport listener failed");
            return;
        }
    };
    let listener = match TcpListener::from_std(std_listener) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(target: "pyronova::server", error = %e, "TPC TcpListener::from_std failed");
            return;
        }
    };

    let tracker = TaskTracker::new();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            res = listener.accept() => {
                match res {
                    Ok((stream, remote_addr)) => {
                        let _ = stream.set_nodelay(true);
                        setup_tcp_quickack(&stream);

                        let routes = Arc::clone(&routes);
                        let conn_token = shutdown.clone();
                        let tls_acc_c = tls_acceptor.clone();
                        tracker.spawn_local(async move {
                            drive_gil_conn(stream, remote_addr, routes, conn_token, tls_acc_c).await;
                        });
                    }
                    Err(e) => handle_accept_error(&e).await,
                }
            }
        }
    }
    tracker.close();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), tracker.wait()).await;
}

async fn drive_gil_conn(
    stream: tokio::net::TcpStream,
    remote_addr: SocketAddr,
    routes: FrozenRoutes,
    conn_token: CancellationToken,
    tls_acceptor: Option<Arc<tokio_rustls::TlsAcceptor>>,
) {
    let tls_stream = match tls_acceptor {
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
    let mut builder = AutoBuilder::new(LocalExec);
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
}

// ---------------------------------------------------------------------------
// Sub-interpreter mode — dispatch still via the old pool for Phase 1
// ---------------------------------------------------------------------------

/// TPC inline mode (Phase 2) — each TPC thread owns its own sub-interp
/// and executes handlers synchronously on the accept thread. No shared
/// pool, no channel, no oneshot wake.
///
/// `workers` must have exactly `n_threads` entries; ownership transfers
/// to the TPC threads. Constraint check: startup must have rejected any
/// gil=True / async def / stream=True route — the inline handler has no
/// path to handle those (see handle_request_tpc_inline).
pub(crate) fn run_tpc_subinterp(
    addr: SocketAddr,
    n_threads: usize,
    n_cpus: usize,
    mut workers: Vec<SubInterpreterWorker>,
    routes: FrozenRoutes,
    tls_acceptor: Option<Arc<tokio_rustls::TlsAcceptor>>,
) -> Result<(), String> {
    if workers.len() != n_threads {
        return Err(format!(
            "TPC worker count mismatch: expected {n_threads}, got {}",
            workers.len()
        ));
    }
    log_startup("hybrid-inline", &addr, n_threads, n_cpus);

    let shutdown = CancellationToken::new();
    let core_ids = core_affinity::get_core_ids().unwrap_or_default();

    let sigint_token = shutdown.clone();
    std::thread::spawn(move || {
        let rt = RuntimeBuilder::new_current_thread()
            .enable_all()
            .build()
            .expect("sigint runtime");
        rt.block_on(async move {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!(target: "pyronova::server", "Shutting down gracefully...");
            println!("\n  Shutting down gracefully...");
            crate::monitor::stop_rss_sampler();
            sigint_token.cancel();
        });
    });

    let mut handles = Vec::with_capacity(n_threads);
    for i in 0..n_threads {
        let core_id = core_ids.get(i).copied();
        let worker = workers.remove(0);
        let routes = Arc::clone(&routes);
        let shutdown = shutdown.clone();
        let tls = tls_acceptor.clone();

        let handle = std::thread::Builder::new()
            .name(format!("pyronova-tpc-{i}"))
            .spawn(move || {
                try_pin_current(core_id);
                let rt = RuntimeBuilder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tpc current-thread runtime build");
                let local = LocalSet::new();
                // Rebind the sub-interp's tstate to THIS OS thread so
                // PyEval_RestoreThread during call_handler targets it.
                // Must happen on the TPC thread itself, not on main.
                let mut worker = worker;
                worker.tstate = unsafe {
                    crate::interp::rebind_tstate_to_current_thread(worker.tstate)
                };
                let worker = std::rc::Rc::new(std::cell::RefCell::new(worker));
                local.block_on(&rt, async move {
                    tpc_accept_loop_inline(addr, worker, routes, shutdown, tls).await;
                });
            })
            .map_err(|e| format!("spawn tpc-{i}: {e}"))?;
        handles.push(handle);
    }

    for h in handles {
        let _ = h.join();
    }
    Ok(())
}

/// GC scheduling mode. Read once at startup from `PYRONOVA_GC_MODE`:
///   - `"count"` (default) — the count-based trigger inside
///     `SubInterpreterWorker::call_handler` fires `gc.collect()` every
///     `PYRONOVA_GC_THRESHOLD` requests per worker. Simple, predictable,
///     can collide with bursty traffic.
///   - `"idle"` — the TPC accept loop fires `gc.collect()` on a
///     100ms-cadence timer, but ONLY when the accept queue drained to
///     empty since the previous tick. Traffic-density-adaptive. A
///     hard failsafe at `PYRONOVA_GC_OOM_FAILSAFE` requests (default
///     50_000) forces a collect even under sustained load so sustained
///     bursts can't starve the collector into OOM. The per-call_handler
///     count trigger is disabled while in this mode.
///   - `"off"` — no framework-level triggers at all. `gc.disable()`
///     still runs at sub-interp init; users must call `gc.collect()`
///     themselves or accept ref-count-only cleanup.
#[derive(Clone, Copy, Debug)]
enum GcMode {
    Count,
    Idle,
    Off,
}

fn gc_mode_from_env() -> GcMode {
    match std::env::var("PYRONOVA_GC_MODE").ok().as_deref() {
        Some("idle") => GcMode::Idle,
        Some("off") => GcMode::Off,
        _ => GcMode::Count,
    }
}

fn oom_failsafe_from_env() -> u64 {
    std::env::var("PYRONOVA_GC_OOM_FAILSAFE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50_000)
}

fn idle_tick_ms_from_env() -> u64 {
    std::env::var("PYRONOVA_GC_IDLE_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100)
}

/// Fire a single `gc.collect()` on the worker's sub-interpreter.
/// Acquires + releases the sub-interp GIL for the duration of the call.
/// Cheap no-op when `gc_collect_func` is null (e.g. `gc` module failed
/// to import at sub-interp init — unreachable in practice).
fn fire_gc(worker: &std::rc::Rc<std::cell::RefCell<SubInterpreterWorker>>) {
    use pyo3::ffi;
    let mut w = worker.borrow_mut();
    if w.gc_collect_func.is_null() {
        return;
    }
    let tstate_cell = std::cell::Cell::new(w.tstate);
    unsafe {
        let _guard = crate::interp::SubInterpGilGuard::acquire(tstate_cell.get(), &tstate_cell);
        // PyObject_CallNoArgs: skip the empty-tuple alloc the generic
        // PyObject_Call path requires. Idiomatic 3.9+ invocation.
        let res = ffi::PyObject_CallNoArgs(w.gc_collect_func);
        if !res.is_null() {
            ffi::Py_DECREF(res);
        } else {
            ffi::PyErr_Clear();
        }
    }
    w.tstate = tstate_cell.get();
}

async fn tpc_accept_loop_inline(
    addr: SocketAddr,
    worker: std::rc::Rc<std::cell::RefCell<SubInterpreterWorker>>,
    routes: FrozenRoutes,
    shutdown: CancellationToken,
    tls_acceptor: Option<Arc<tokio_rustls::TlsAcceptor>>,
) {
    let std_listener = match create_reuseport_listener(addr) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(target: "pyronova::server", error = %e, "TPC reuseport listener failed");
            return;
        }
    };
    let listener = match TcpListener::from_std(std_listener) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(target: "pyronova::server", error = %e, "TPC TcpListener::from_std failed");
            return;
        }
    };

    let gc_mode = gc_mode_from_env();
    // In idle mode, disable the count-based trigger inside call_handler —
    // we drive GC from the accept loop instead. Each TPC thread owns its
    // worker exclusively so the borrow_mut is free of contention.
    if matches!(gc_mode, GcMode::Idle | GcMode::Off) {
        worker.borrow_mut().gc_threshold = 0;
    }
    let oom_failsafe = oom_failsafe_from_env();
    let idle_ms = idle_tick_ms_from_env();

    let tracker = TaskTracker::new();
    match gc_mode {
        GcMode::Idle => {
            // Hybrid idle trigger: collect either on every idle tick
            // (when the accept queue drained since last tick) or on a
            // hard failsafe to prevent OOM during sustained bursts.
            //
            // `requests_since_last_gc` is written from the accept path
            // (one TPC thread — no atomics needed; the borrow is single-
            // threaded on this current_thread runtime).
            let mut requests_since_last_gc: u64 = 0;
            let mut gc_timer =
                tokio::time::interval(std::time::Duration::from_millis(idle_ms));
            // First tick fires immediately — skip it so we don't collect
            // an empty heap before any requests have run.
            gc_timer.tick().await;
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => break,
                    res = listener.accept() => {
                        match res {
                            Ok((stream, remote_addr)) => {
                                let _ = stream.set_nodelay(true);
                                setup_tcp_quickack(&stream);
                                requests_since_last_gc += 1;

                                let worker_clone = std::rc::Rc::clone(&worker);
                                let routes = Arc::clone(&routes);
                                let conn_token = shutdown.clone();
                                let tls_acc_c = tls_acceptor.clone();
                                tracker.spawn_local(async move {
                                    drive_inline_conn(stream, remote_addr, worker_clone, routes, conn_token, tls_acc_c).await;
                                });

                                // Failsafe: sustained burst — accept
                                // queue never drains, so the idle tick
                                // never gets its chance. Force a
                                // collect here to keep RSS bounded.
                                if requests_since_last_gc >= oom_failsafe {
                                    fire_gc(&worker);
                                    requests_since_last_gc = 0;
                                    gc_timer.reset();
                                }
                            }
                            Err(e) => handle_accept_error(&e).await,
                        }
                    }
                    _ = gc_timer.tick(), if requests_since_last_gc > 0 => {
                        // Idle tick: the select! raced the tick against
                        // accept() and the tick won — the accept queue
                        // was quiet for at least the tick period. Fire
                        // the collect without interrupting any user-
                        // visible request.
                        fire_gc(&worker);
                        requests_since_last_gc = 0;
                    }
                }
            }
        }
        GcMode::Count | GcMode::Off => {
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => break,
                    res = listener.accept() => {
                        match res {
                            Ok((stream, remote_addr)) => {
                                let _ = stream.set_nodelay(true);
                                setup_tcp_quickack(&stream);

                                let worker_clone = std::rc::Rc::clone(&worker);
                                let routes = Arc::clone(&routes);
                                let conn_token = shutdown.clone();
                                let tls_acc_c = tls_acceptor.clone();
                                tracker.spawn_local(async move {
                                    drive_inline_conn(stream, remote_addr, worker_clone, routes, conn_token, tls_acc_c).await;
                                });
                            }
                            Err(e) => handle_accept_error(&e).await,
                        }
                    }
                }
            }
        }
    }
    tracker.close();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), tracker.wait()).await;
}

async fn drive_inline_conn(
    stream: tokio::net::TcpStream,
    remote_addr: SocketAddr,
    worker: std::rc::Rc<std::cell::RefCell<SubInterpreterWorker>>,
    routes: FrozenRoutes,
    conn_token: CancellationToken,
    tls_acceptor: Option<Arc<tokio_rustls::TlsAcceptor>>,
) {
    let tls_stream = match tls_acceptor {
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
        let worker = std::rc::Rc::clone(&worker);
        let routes = Arc::clone(&routes);
        let client_ip_addr = remote_addr.ip();
        async move {
            if websocket::is_websocket_upgrade(&req) {
                websocket::handle_websocket(req, routes).await
            } else {
                handle_request_tpc_inline(req, routes, worker, client_ip_addr).await
            }
        }
    });
    let mut builder = AutoBuilder::new(LocalExec);
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
}

// ---------------------------------------------------------------------------
// Flag
// ---------------------------------------------------------------------------

/// Env-var gate for Phase 1. The Python-side `app.run(tpc=True)` kwarg
/// goes through a different path in `PyronovaApp` and does not consult
/// this function.
/// Count physical CPU cores (not logical hyperthreads). Parses
/// /sys/devices/system/cpu/cpu*/topology/thread_siblings_list — the
/// number of unique sibling groups equals the physical core count.
/// Falls back to logical core count if topology is unavailable.
pub(crate) fn physical_core_count() -> usize {
    use std::collections::HashSet;
    use std::fs;

    let Ok(entries) = fs::read_dir("/sys/devices/system/cpu") else {
        return std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
    };

    let mut sibling_groups: HashSet<String> = HashSet::new();
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("cpu") || !name[3..].chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let path = e.path().join("topology/thread_siblings_list");
        if let Ok(s) = fs::read_to_string(&path) {
            sibling_groups.insert(s.trim().to_string());
        }
    }
    if sibling_groups.is_empty() {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    } else {
        sibling_groups.len()
    }
}

pub(crate) fn env_enabled() -> bool {
    matches!(
        std::env::var("PYRONOVA_TPC").ok().as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}
