# Pyre Logging System Design

## Overview

Pyre's logging system is built on two principles:
1. **All I/O sinks to Rust** — Python never touches stdout/stderr directly for logs
2. **Zero-cost when off** — `tracing` macros compile to an atomic level-check; when filtered out, no string formatting, no syscalls, no overhead

---

## Architecture

```
                    ┌─────────────────────────────────────┐
                    │         Rust tracing-subscriber      │
                    │   (EnvFilter + fmt::Layer)           │
                    │   targets: pyre::server              │
                    │            pyre::access              │
                    │            pyre::app                 │
                    └──────┬─────────┬──────────┬──────────┘
                           │         │          │
              ┌────────────┘         │          └────────────┐
              │                      │                       │
     ┌────────▼────────┐   ┌────────▼────────┐    ┌────────▼────────┐
     │  Server Log     │   │  Access Log     │    │  App Log        │
     │  pyre::server   │   │  pyre::access   │    │  pyre::app      │
     │                 │   │                 │    │                 │
     │  - Startup      │   │  - method       │    │  - Python       │
     │  - Shutdown     │   │  - path         │    │    logging.*    │
     │  - GIL watchdog │   │  - status       │    │  - worker_id   │
     │  - WS errors    │   │  - latency_us   │    │  - logger name │
     │  - Conn errors  │   │  - mode         │    │  - file:line   │
     └─────────────────┘   └─────────────────┘    └─────────────────┘
           Rust only            Rust only           Python → Rust FFI
```

---

## Three Log Targets

### 1. `pyre::server` — Server Lifecycle

Startup, shutdown, watchdog alerts, connection errors.

```
INFO  pyre::server Pyre started version="1.2.0" mode="hybrid" addr=127.0.0.1:8000
INFO  pyre::server Shutting down gracefully...
WARN  pyre::server GIL watchdog: main GIL congested latency_ms=52
WARN  pyre::server Connection error error="connection reset by peer"
ERROR pyre::server WebSocket upgrade error error="..."
```

### 2. `pyre::access` — Request Access Log

Every HTTP request with method, path, status, latency, and execution mode.

```
INFO  pyre::access Request handled method=GET path=/ status=200 latency_us=198 mode="subinterp"
INFO  pyre::access Request handled method=POST path=/api/users status=201 latency_us=1542 mode="gil"
WARN  pyre::access Client error method=GET path=/missing status=404 latency_us=12
ERROR pyre::access Request failed method=POST path=/crash status=500 latency_us=892
```

### 3. `pyre::app` — Python Application Logs

User code `logging.info()` calls bridged from Python to Rust via FFI.

```
INFO  pyre::app Fetching users from DB worker=3 logger=myapp file=app.py line=42
ERROR pyre::app Database connection failed worker=7 logger=db file=models.py line=88
```

---

## Configuration API

```python
from pyreframework import Pyre

# 1. Debug mode — full output, human-readable text
app = Pyre(debug=True)

# 2. Production — errors only, JSON for ELK/Datadog
app = Pyre()  # defaults: level=ERROR, access_log=False, format=json

# 3. Custom — fine-grained control
app = Pyre(log_config={
    "level": "INFO",        # OFF, ERROR, WARN, INFO, DEBUG, TRACE
    "access_log": True,     # enable per-request logging
    "format": "json",       # json | text
})

# 4. Silent mode — absolute zero overhead for benchmarks
app = Pyre(log_config={"level": "OFF"})

# 5. enable_logging() — activates access log + Python hook output
app = Pyre()
app.enable_logging()       # upgrades level to INFO, enables access_log
```

### Environment Variables

| Variable | Effect |
|---|---|
| `PYRE_LOG=1` | Auto-enable logging (equivalent to `app.enable_logging()`) |
| `PYRE_METRICS=1` | Enable GIL watchdog (10ms probe interval) |

---

## Python Logging Bridge

### Problem

Python's default `logging.StreamHandler` does synchronous `write()` to stderr while holding the GIL. At 220k QPS, this destroys throughput.

### Solution

Pyre hijacks Python's root logger in both the main interpreter and every sub-interpreter:

**Main interpreter** (`app.py`):
```python
class PyreRustHandler(logging.Handler):
    def emit(self, record):
        emit_python_log(           # PyO3 FFI → Rust
            level=record.levelname,
            name=record.name,
            message=record.getMessage(),
            pathname=record.pathname,
            lineno=record.lineno,
        )
```

**Sub-interpreters** (`_bootstrap.py`):
```python
class _PyreRustHandler(logging.Handler):
    def emit(self, record):
        _pyre_emit_log(            # C-FFI → Rust (registered like pyre_recv/pyre_send)
            record.levelname,
            record.name,
            record.getMessage(),
            record.pathname or "",
            record.lineno or 0,
            self._worker_id,
        )
```

### Performance Characteristics

| Scenario | Cost |
|---|---|
| `level=OFF` | ~1ns (atomic compare, branch predicted skip) |
| `level=INFO`, log filtered out | ~1ns (same) |
| `level=INFO`, log accepted | ~50-100ns FFI crossing + tracing format |
| Python `logger.info("msg")` | ~200ns (getMessage + FFI) |
| Python `logger.info("data: %s", huge_dict)` | Cost of `%s` formatting (unavoidable) |

---

## Rust Implementation

### Files Modified

| File | Changes |
|---|---|
| `Cargo.toml` | Added `tracing`, `tracing-subscriber` (env-filter, json) |
| `src/logging.rs` | **New** — `init_logger()`, `emit_python_log()` PyO3 functions |
| `src/lib.rs` | Registered `logging` module + functions |
| `src/app.rs` | Startup/shutdown → `tracing::info!`, conn errors → `tracing::warn!` |
| `src/handlers.rs` | Access log with `latency_us`, `method`, `path`, `status`, `mode` |
| `src/interp.rs` | `pyre_emit_log_cfunc` C-FFI, registered in all sub-interpreters |
| `src/monitor.rs` | GIL watchdog → `tracing::warn!` |
| `src/websocket.rs` | WS errors → `tracing::error!`/`tracing::warn!` |

### Key Design Decisions

1. **`EnvFilter` for zero-cost OFF** — When level is OFF or filtered, `tracing::info!` compiles to a single atomic load + branch. CPU branch predictor hits 100% after warmup.

2. **Separate `pyre::access` target** — Allows users to disable access log while keeping server/app logs, or vice versa. Controlled by `access_log` config flag mapped to `pyre::access=off` directive.

3. **C-FFI bridge for sub-interpreters** — Sub-interpreters can't import PyO3 extension modules. `_pyre_emit_log` is registered as a C-FFI built-in function (like `pyre_recv`/`pyre_send`), injected into globals before bootstrap runs.

4. **Deferred `init_logger` to `run()`** — Allows `enable_logging()` to modify log config before the tracing subscriber is locked in. `tracing-subscriber` only allows one initialization per process.

5. **`println!` retained for startup banner** — The human-readable startup banner (`Pyre v1.2.0 [hybrid mode]...`) is kept as `println!` alongside `tracing::info!` because it's always-visible DX, not filterable log output.

---

## Testing

`tests/test_logging.py` covers 8 scenarios:

| Test | What it verifies |
|---|---|
| `test_gil_mode_logging` | Python hook output (`[INFO ] GET / → 200`) in GIL mode |
| `test_subinterp_rust_logging` | Rust tracing access log in subinterp mode |
| `test_user_print_in_subinterp` | `print()` works in sub-interpreter handlers |
| `test_user_logging_in_subinterp` | Python `logging.info()` bridges to Rust tracing |
| `test_debug_mode_tracing` | `debug=True` produces server lifecycle tracing |
| `test_debug_mode_access_log` | `debug=True` produces access log with latency |
| `test_python_logging_bridge_main` | Main interpreter logging bridge works |
| `test_json_format` | JSON format output with structured fields |
