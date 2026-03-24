# SkyTrade Engine (Pyre)

High-performance Python web framework powered by Rust. Goal: outperform Robyn.

## Architecture

- **Rust core** (`src/`): Modular — 9 files, each under 350 lines
  - `lib.rs` — module declarations + `#[pymodule]` (18 lines)
  - `types.rs` — `SkyRequest`, `SkyResponse`, `extract_headers`
  - `app.rs` — `SkyApp` with `run_gil()` / `run_subinterp()`
  - `handlers.rs` — GIL and sub-interpreter request handlers
  - `router.rs` — `RouteTable` + `SharedRoutes`
  - `response.rs` — response builders + `extract_response_data`
  - `json.rs` — Rust-side `py_to_json_value` serializer
  - `static_fs.rs` — async static file serving + MIME detection
  - `interp.rs` — `PyObjRef` RAII, channel-based worker pool, AST filter
- **Python interface** (`python/skytrade/`):
  - `engine` (Rust): `SkyApp`, `SkyRequest`, `SkyResponse`
  - `app.py`: `Pyre` class — decorator-friendly wrapper over `SkyApp`
- **Build**: Maturin (mixed python/rust project), module name `skytrade.engine`

## Development

```bash
# Setup
python3 -m venv .venv && source .venv/bin/activate
pip install maturin

# Build (release mode)
maturin develop --release

# Run example
python examples/hello.py

# Benchmark (requires wrk: brew install wrk)
bash benchmarks/run_bench.sh
```

## Key Design Decisions

- Route table uses index-based lookup (Vec<Py<PyAny>> + Router<usize>) to avoid Py<PyAny> Clone issues in PyO3 0.28
- GIL released via `py.detach()` during Tokio event loop, reacquired via `Python::attach()` per-request for handler calls
- `#[pyclass(frozen)]` on SkyRequest/SkyResponse for thread safety — no mutable attributes
- `Pyre` Python wrapper provides decorator syntax; `SkyApp` is the raw Rust engine
- Sub-interpreter mode uses `crossbeam-channel` multi-consumer pool with `tokio::sync::oneshot` async responses
- `PyObjRef` RAII wrapper for all raw FFI pointer operations — Drop auto-DECREFs
- AST-based script filtering (`ast.parse` + `ast.unparse`) for sub-interpreter bootstrap
- Static files served via Tokio async fs — no GIL needed
- Middleware: before_request/after_request hooks stored in RouteTable alongside route handlers
- WebSocket: tokio-tungstenite async ↔ Python sync via dual channels, one OS thread per connection

## Project Structure

```
src/
  lib.rs              # Module declarations + #[pymodule] (18 lines)
  types.rs            # SkyRequest, SkyResponse, extract_headers
  app.rs              # SkyApp — route registration + server startup
  handlers.rs         # handle_request (GIL), handle_request_subinterp (channel)
  router.rs           # RouteTable, SharedRoutes
  response.rs         # Response builders, extract_response_data
  json.rs             # py_to_json_value
  static_fs.rs        # try_static_file, mime_from_ext
  interp.rs           # PyObjRef RAII, channel worker pool, AST filter
  websocket.rs        # SkyWebSocket, upgrade handler, async↔sync bridge
python/skytrade/
  __init__.py         # Re-exports Pyre, SkyApp, SkyRequest, SkyResponse
  app.py              # Pyre class (decorator syntax, logging, static files)
examples/hello.py     # Demo app with decorators + middleware
benchmarks/
  run_bench.sh        # Head-to-head benchmark vs Robyn
  robyn_app.py        # Robyn equivalent for comparison
  bench.py            # Standalone wrk runner
  benchmark-*.md      # Benchmark results history
```
