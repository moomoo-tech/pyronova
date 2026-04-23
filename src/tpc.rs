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

                        let worker = std::rc::Rc::clone(&worker);
                        let routes = Arc::clone(&routes);
                        let conn_token = shutdown.clone();
                        let tls_acc_c = tls_acceptor.clone();
                        tracker.spawn_local(async move {
                            drive_inline_conn(stream, remote_addr, worker, routes, conn_token, tls_acc_c).await;
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
pub(crate) fn env_enabled() -> bool {
    matches!(
        std::env::var("PYRONOVA_TPC").ok().as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}
