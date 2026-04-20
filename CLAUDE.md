# Pyre 🔥

High-performance Python web framework powered by Rust. Per-Interpreter GIL (PEP 684) for true multi-core parallelism in a single process.

**Benchmarks**: 420k req/s (Linux), 222 MB memory. 15x faster than Robyn, 300s sustained 400k QPS zero errors.

## Architecture

- **Rust core** (`src/`): 12 modules
  - `lib.rs` — module declarations, `#[pymodule]`, mimalloc global allocator
  - `types.rs` — `PyreRequest`, `PyreResponse`, `extract_headers`
  - `app.rs` — `PyreApp` with `run_gil()` / `run_subinterp()`, graceful shutdown
  - `handlers.rs` — GIL handler, sub-interp handler (30s zombie timeout), streaming
  - `router.rs` — `RouteTable`, `MutableRoutes`, `FrozenRoutes`
  - `response.rs` — response builders (200/404/413/500/503/504)
  - `json.rs` — Rust-side `py_to_json_value` serializer
  - `static_fs.rs` — async static file serving + MIME detection + path traversal protection
  - `interp.rs` — `PyObjRef` RAII, C-FFI bridge (`pyre_recv`/`pyre_send`), dual worker pool (sync+async), mock module injection
  - `websocket.rs` — WebSocket upgrade, `PyreWebSocket` pyclass, async↔sync bridge
  - `stream.rs` — `PyreStream` SSE with mpsc channel
  - `logging.rs` — `init_logger` (tracing-subscriber), `emit_python_log` (Python→Rust bridge)
  - `monitor.rs` — GIL watchdog, memory RSS, atomic counters
  - `state.rs` — `SharedState` backed by `Arc<DashMap>`
- **Python interface** (`python/pyreframework/`):
  - `engine` (Rust): `PyreApp`, `PyreRequest`, `PyreResponse`, `PyreWebSocket`, `SharedState`, `PyreStream`
  - `app.py`: `Pyre` class — decorators, CORS, logging, Pydantic model=, env var config, hot reload, dual pool auto-detection
  - `mcp.py`: MCP server (JSON-RPC 2.0) with tool/resource/prompt decorators
  - `rpc.py`: MsgPack RPC + `PyreRPCClient` magic client
  - `cookies.py`: Cookie get/set/delete utilities
  - `uploads.py`: Multipart form-data parser
  - `testing.py`: `TestClient` for tests without a running server
  - `_async_engine.py`: Async engine script injected into sub-interpreter workers
  - `engine.pyi`: Type stubs for IDE autocomplete
- **Build**: Maturin (mixed python/rust project), module name `pyreframework.engine`

## Development

```bash
# Setup
python3 -m venv .venv && source .venv/bin/activate
pip install maturin

# Build (release mode)
maturin develop --release

# Run example
python examples/hello.py

# Run tests
uv pip install -e ".[test]"
pytest tests/ --ignore=tests/test_ws_binary_client.py -q

# Benchmark vs FastAPI (requires wrk: brew install wrk)
bash benchmarks/run_comparison.sh

# Benchmark vs Robyn
bash benchmarks/run_bench.sh
```

## Key Design Decisions

- Route table uses index-based lookup (`Vec<Py<PyAny>>` + `Router<usize>`) to avoid `Py<PyAny>` Clone issues in PyO3 0.28
- GIL released via `py.detach()` during Tokio event loop, reacquired via `Python::attach()` per-request
- `#[pyclass(frozen)]` on PyreRequest/PyreResponse for thread safety
- `Pyre` Python wrapper provides decorator syntax; `PyreApp` is the raw Rust engine
- Sub-interpreter mode uses `crossbeam-channel` multi-consumer pool with `tokio::sync::oneshot` async responses
- `PyObjRef` RAII wrapper for all raw FFI pointer operations — Drop auto-DECREFs
- C-FFI bridge (`pyre_recv`/`pyre_send`) for native async in sub-interpreters — releases GIL during channel wait
- Hybrid dispatch: `gil=True` routes go to main interpreter (for C extensions), others to sub-interpreters
- Auto dual-pool: framework detects `async def` vs `def` handlers, routes to appropriate worker pool
- Mock module injection in sub-interpreters for pydantic/pyreframework submodules
- Static files served via Tokio async fs — no GIL needed
- Middleware: before_request/after_request hooks stored in RouteTable
- WebSocket: tokio-tungstenite async ↔ Python sync via dual channels, one OS thread per connection
- SSE: `PyreStream` with mpsc unbounded channel, returned from handler
- Logging: Rust `tracing` with `EnvFilter` (zero-cost OFF), three targets (`pyre::server`, `pyre::access`, `pyre::app`), Python logging hijacked via C-FFI bridge in sub-interpreters
- mimalloc global allocator for high-concurrency allocation performance
- 30s zombie request timeout in sub-interpreter mode (504 Gateway Timeout)
- Graceful shutdown via `signal::ctrl_c()` + `tokio::select!`

## Project Structure

```
src/
  lib.rs              # Module declarations + #[pymodule] + mimalloc
  logging.rs          # Rust tracing engine + Python logging bridge
  types.rs            # PyreRequest, PyreResponse, extract_headers
  app.rs              # PyreApp — route registration + server startup
  handlers.rs         # handle_request (GIL), handle_request_subinterp (channel)
  router.rs           # RouteTable, MutableRoutes, FrozenRoutes
  response.rs         # Response builders, extract_response_data
  json.rs             # py_to_json_value
  static_fs.rs        # try_static_file, mime_from_ext
  interp.rs           # PyObjRef RAII, C-FFI bridge, dual worker pool, mock injection
  websocket.rs        # PyreWebSocket, upgrade handler, async↔sync bridge
  stream.rs           # PyreStream SSE
  monitor.rs          # GIL watchdog, memory RSS, atomic counters
  state.rs            # SharedState (DashMap)
python/pyreframework/
  __init__.py         # Re-exports all public APIs
  app.py              # Pyre class (decorators, CORS, logging, config)
  mcp.py              # MCP server (JSON-RPC 2.0)
  rpc.py              # MsgPack RPC + PyreRPCClient
  cookies.py          # Cookie utilities
  uploads.py          # Multipart form-data parser
  testing.py          # TestClient
  _async_engine.py    # Async engine script for sub-interpreters
  engine.pyi          # Type stubs
examples/
  hello.py            # Basic demo
  ai_agent_server.py  # MCP + SSE + SharedState + Pydantic
  trading_api.py      # numpy + WebSocket + RPC + SharedState
  fullstack_api.py    # CRUD + Cookie auth + file upload
tests/
  test_all_features.py      # 22 tests (11 per mode × 2)
  test_async_isolation.py   # Proves async isolation
  test_logging.py           # 4 logging tests
  test_env_var_worker.py    # Env var + decorator tests
  test_async_bridge.py      # Phase 7.2 async bridge
  e2e/                      # Manual-run drivers (ws_binary_server/client)
benchmarks/
  run_comparison.sh   # Pyre vs FastAPI head-to-head
  run_bench.sh        # Pyre vs Robyn
  benchmark-*.md      # Results history (14 reports)
docs/
  subinterp-safe-ecosystem.md  # Golden Path ecosystem guide
  phase-7.2-async-bridge.md    # Native async bridge design
  dual-engine-design.md        # Dual pool architecture
  gil-monitor-design.md        # GIL watchdog design
  gc-optimization-guide.md     # GC tuning
  logging-design.md            # 日志系统设计（中文）
  logging-design.en.md         # Logging system design (English)
  zero-copy-design.md          # Zero-copy design
  rpc-engine-design.md         # RPC engine design
  why-not-multiprocess.md      # Architecture rationale
  developer-experience.md      # DX philosophy
  subinterp-c-extension-compat.md  # C extension compatibility
```
