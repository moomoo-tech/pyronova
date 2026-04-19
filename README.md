# Pyre 🔥

**High-performance Python web framework powered by Rust.**

Built on Per-Interpreter GIL (PEP 684) and a Rust async core, Pyre runs Python handlers across all CPU cores in a single process.

- **902k req/s** on Linux (AMD Ryzen 7840HS, 8C/16T) under TechEmpower-style
  pipelined plaintext (`wrk -t8 -c256 --pipeline 16`).
- **423k req/s** on the standard single-route baseline (`wrk -t4 -c100`,
  no pipeline), **+0.8% vs v1.4.0** on the same hardware.
- **2.7× faster than Robyn** at equal scale (16 workers each), **1/3 the memory**.
- Sustained **400k QPS**: RSS grew **4 MB over 73.8M requests** in 180s
  (≈0 B/req). Zero errors, zero leaks.

### What's new in v1.5.0

- **Sub-interpreter memory-leak root cause closed.** Unbounded RSS growth
  under sustained load, present since v1.4.0, traced to cross-thread
  `PyThreadState` reuse and fixed via `PyThreadState_New` per worker.
  See `docs/advisor-triage-2026-04-19.md` and
  [CHANGELOG](CHANGELOG.md#v150-2026-04-19) for the full writeup.
- **Raw C-API `_PyreRequest` type** built via `PyType_FromSpec` with
  a deterministic Rust-owned `tp_dealloc` — replaces the Python-class
  stub, closes a CPython `subtype_dealloc` hazard, restores proper
  `__del__` / `tp_finalize` semantics in sub-interp handlers.
- **22 correctness / hygiene fixes** from an adversarial review pass
  (C-API reentrancy, graceful shutdown, TOCTOU in static file serving,
  CORS misconfig warnings, URL-decoded path params, async middleware
  driving, …).
- **Awaitable detection via `tp_as_async->am_await` pointer probe**
  replaces the μs-level `PyObject_HasAttrString("__await__")`.
- **Benchmark infrastructure split** — `just bench-record` / `bench-compare`
  gate on a minimal plaintext target; `just bench-features` measures
  the full demo; `just bench-tfb-plaintext` runs TechEmpower pipeline=16.
- **Breaking:** `requires-python = ">=3.13"` (was `>=3.10`). Uses
  `PyThreadState_GetUnchecked` added in CPython 3.13.

Full benchmark (v1.4.0 baseline, unchanged methodology):
[benchmarks/benchmark-14-linux.en.md](benchmarks/benchmark-14-linux.en.md)

### What others can't do, Pyre has built-in

- **SharedState** — cross-worker memory sharing without Redis (nanosecond latency)
- **AI-native** — MCP server, MsgPack RPC, SSE streaming
- **Observable** — GIL watchdog, backpressure (503), request timeout (504)

```python
from pyreframework import Pyre

app = Pyre()

@app.get("/")
def index(req):
    return {"hello": "world"}

@app.get("/io")
async def io_heavy(req):
    import asyncio
    await asyncio.sleep(0.1)
    return "done"

app.run()
```

## Why Pyre?

### The problem

AI applications in Python need **high throughput and low memory**. An AI agent backend handles thousands of concurrent LLM calls, RAG queries, and tool invocations — all I/O-heavy, all in Python. A quantitative trading gateway processes hundreds of real-time data feeds simultaneously. These workloads demand the performance of C++ with the ecosystem of Python.

But Python has the GIL (Global Interpreter Lock). One lock, one core, no parallelism. Every framework before Pyre works around this with compromises.

### What others do (and why it's not enough)

**FastAPI** chose async on a single core. Elegant for I/O, but one CPU-heavy request (JSON parsing, Pydantic validation, numpy computation) blocks the entire event loop. Scale via Gunicorn means duplicating the full Python runtime per process. At 15k req/s, you hit the ceiling.

**Robyn** replaced the Python event loop with Rust (Tokio). Better I/O, but Python handlers still run on one GIL. Scaling means 22+ OS processes (`--fast`), eating 447 MB. The Rust layer is fast; the Python layer is the bottleneck.

**Multi-threading** doesn't help. Python threads share one GIL — they take turns, not run in parallel. Adding threads adds context-switch overhead without adding throughput. `threading` in Python is concurrency theater, not parallelism.

**Free-threaded Python** (no-GIL, PEP 703) removes the lock but makes every Python object operation slower (atomic reference counting). The ecosystem isn't ready — most C extensions assume the GIL exists. It trades one problem for another.

### What Pyre does differently

**Pyre multiplies the GIL.** Using Per-Interpreter GIL (PEP 684), each worker gets its own independent Python interpreter with its own GIL inside a single process. True multi-core parallelism, zero memory duplication, zero IPC overhead.

```
FastAPI:  1 process × 1 GIL × async tricks     = fast I/O, slow CPU, 15k QPS
Robyn:    16 processes × 16 GILs × 16× memory  = brute force, 156k QPS, 583 MB
Pyre:     1 process × 16 GILs × shared memory   = elegant, 429k QPS, 189 MB
```

This matters for AI:
- **LLM gateway** — thousands of concurrent `await` calls, each taking 2-5 seconds. Pyre's async pool handles 133k concurrent I/O operations.
- **Agent orchestration** — multiple agents computing simultaneously. Each sub-interpreter runs at full CPU speed without blocking others.
- **Memory efficiency** — deploy 3x more instances on the same hardware. Pyre 189 MB for 16 workers vs Robyn 583 MB. On a 512 MB container, Pyre runs 16 parallel workers; Robyn fits 12 at best.
- **State sharing** — cross-worker `app.state` with nanosecond latency. No Redis, no serialization, no network hop. Session management, caching, and coordination built into the framework.

## Performance

Benchmarked on Linux (AMD Ryzen 7 7840HS, 8C/16T), Python 3.12, wrk -t4 -c100 -d10s.

Full report: [benchmarks/benchmark-14-linux.md](benchmarks/benchmark-14-linux.md)

### Throughput (requests/sec)

| Route | Pyre | P50 | P99 |
|-------|------|-----|-----|
| GET / (plain text) | **429,000** | 185μs | 579μs |
| GET /json | **405,000** | 196μs | 599μs |
| GET /user/42 (path param) | **395,000** | 201μs | 641μs |
| POST /echo (JSON parse) | **372,000** | 214μs | 785μs |
| GET /compute (CPU-bound) | **379,000** | 211μs | 772μs |

### Pyre vs Robyn (fair comparison)

Both frameworks given 16 workers on the same hardware. Robyn: `--processes 16 --workers 2`.

| Route | Pyre (1 proc, 16 sub-interp) | Robyn (16 proc × 2 workers) | Ratio |
|-------|-----|-------|-------|
| GET / | **429k** req/s | 156k req/s | **2.7x** |
| GET /json | **405k** req/s | 155k req/s | **2.6x** |
| GET /user/42 | **395k** req/s | 144k req/s | **2.7x** |
| POST /echo | **372k** req/s | 144k req/s | **2.6x** |
| GET /compute | **379k** req/s | 145k req/s | **2.6x** |

### Resource efficiency

| Resource | Pyre | Robyn (16 proc) |
|----------|------|-----------------|
| Memory | **189 MB** | 583 MB |
| Processes | **1** | 16 |
| QPS per MB | **2,268 req/s/MB** | 268 req/s/MB |
| Cross-worker state | Built-in (DashMap, nanosecond) | Needs Redis |

Pyre achieves **2.7x the throughput with 1/3 the memory**. Per-MB efficiency is **8.5x** better.

### Stability (5-minute sustained load)

Sustained 300s stress test, wrk -t4 -c100.

| Metric | Linux (v1.4.0) | macOS (v1.2.0) |
|--------|----------------|----------------|
| Sustained QPS | **400,683 req/s** | 214,641 req/s |
| Total requests | **120,209,758** (120M) | 64,410,189 (64M) |
| Non-2xx responses | **0** | 0 |
| Socket errors | **0** | 0 |
| Memory growth | 169 → 196 MB (+27 MB) | 1712 → 752 KB |
| Max latency | 9.02ms | 39.98ms |

120 million requests, zero errors, zero memory leaks.

### Stress test (c=1024)

| Metric | Result |
|--------|--------|
| QPS | **356,328 req/s** |
| P50 / P99 | 1.40ms / 3.15ms |
| Errors | **0** |

Graceful degradation under extreme concurrency — still 356k QPS with zero errors.

### Pyre vs Robyn: feature comparison

| Capability | Pyre | Robyn |
|------------|------|-------|
| **Architecture** | 1 process, N sub-interpreters | N OS processes |
| **SharedState** (cross-worker) | Built-in (DashMap, nanosecond) | Not supported (needs Redis) |
| **MCP Server** (AI tool protocol) | Built-in | Supported (experimental) |
| **MsgPack RPC** | Built-in + magic client | Not supported |
| **SSE Streaming** | Built-in (PyreStream) | Supported |
| **GIL Watchdog** | Built-in (contention + hold time) | Not supported |
| **Backpressure** (503 overload) | Built-in (bounded channels) | Not supported |
| **Request Timeout** (504) | Built-in (30s zombie reaper) | Not supported |
| **Hybrid Dispatch** (`gil=True`) | Auto-routes to main interpreter | Not supported |
| **TestClient** | Built-in | Not built-in |
| **WebSocket** | Supported | Supported |
| **CORS** | Supported | Supported |
| **Static Files** | Supported (async, no GIL) | Supported |
| **Middleware** | before/after hooks | Supported |
| **Hot Reload** | Supported | Supported |

### Who is Pyre for?

**AI Agent servers** — Build MCP-compatible tool servers, LLM gateways, and multi-agent orchestration backends. Handle thousands of concurrent LLM streaming responses with SSE. SharedState coordinates agents without Redis.

**Quantitative trading** — Process real-time market data feeds with sub-millisecond P50 latency. Sub-interpreter parallelism runs strategy computations across all cores without GIL contention. WebSocket support for live order book streaming.

**High-throughput microservices** — Internal service mesh nodes that need maximum req/s with minimum memory. MsgPack RPC for binary-efficient inter-service communication. Backpressure (503) protects downstream systems under load spikes.

**Edge/IoT gateways** — Run on memory-constrained devices (512 MB containers, Raspberry Pi). 67 MB for 10 parallel workers vs 447 MB for the alternatives.

## How Pyre works

```
┌─────────────────────────────────────────────────────────┐
│                    Pyre Architecture                     │
├─────────────────────────────────────────────────────────┤
│  Python handlers (def / async def / gil=True)           │
│      ↓                                                   │
│  Rust core (Tokio + Hyper)                              │
│  ├── Sync worker pool ──→ N sub-interpreters (OWN_GIL)  │
│  ├── Async worker pool ──→ N asyncio event loops        │
│  ├── Hybrid dispatch ──→ main interpreter (numpy/C ext) │
│  ├── SharedState ──→ DashMap (nanosecond, cross-worker) │
│  └── Backpressure ──→ bounded channels (503 on overload)│
└─────────────────────────────────────────────────────────┘
```

## Feature Comparison

### Routing & Request/Response

| Feature | Pyre | FastAPI | Robyn |
|---------|------|---------|-------|
| Decorator routing | ✅ | ✅ | ✅ |
| Path params `/hello/{name}` | ✅ | ✅ | ✅ |
| Query params | ✅ | ✅ | ✅ |
| JSON parsing | ✅ | ✅ | ✅ |
| Pydantic validation | ✅ `model=` | ✅ native | ✅ |
| File upload (multipart) | ✅ | ✅ | ✅ |
| Cookie read/write | ✅ | ✅ | ✅ |
| Redirect | ✅ | ✅ | ✅ |
| Custom status/headers | ✅ | ✅ | ✅ |
| Static files | ✅ | ✅ | ✅ |

### Protocols

| Feature | Pyre | FastAPI | Robyn |
|---------|------|---------|-------|
| HTTP/1.1 | ✅ | ✅ | ✅ |
| HTTP/2 | ✅ | ✅ (Hypercorn) | ✅ |
| WebSocket (text+binary) | ✅ | ✅ | ✅ |
| SSE streaming | ✅ | ✅ | ✅ |

### Middleware & Security

| Feature | Pyre | FastAPI | Robyn |
|---------|------|---------|-------|
| before/after hooks | ✅ | ✅ middleware | ✅ |
| CORS | ✅ built-in | ✅ | ✅ |
| Body size limit | ✅ 10MB | ✅ | ✅ |
| Backpressure (503) | ✅ | ❌ | ❌ |
| Path traversal protection | ✅ | ✅ | ❌ |
| Worker panic protection | ✅ catch_unwind | N/A | N/A |

### AI & Microservices

| Feature | Pyre | FastAPI | Robyn |
|---------|------|---------|-------|
| MCP Server (AI tools) | ✅ native | ❌ (third-party) | ✅ |
| MsgPack RPC | ✅ | ❌ | ❌ |
| Content negotiation | ✅ JSON/MsgPack | JSON only | JSON only |
| Magic RPC Client | ✅ | ❌ | ❌ |
| SharedState (no Redis) | ✅ nanosecond | ❌ needs Redis | ❌ needs Redis |

### Concurrency (Pyre unique)

| Feature | Pyre | FastAPI | Robyn |
|---------|------|---------|-------|
| Sub-interpreter parallelism | ✅ Per-GIL | ❌ | ❌ |
| Hybrid GIL dispatch | ✅ | ❌ | ❌ |
| Auto sync/async dual pool | ✅ zero-loss | ❌ | ❌ |
| Multi-process | — (not needed) | ✅ Gunicorn | ✅ --fast |

### Observability (Pyre unique)

| Feature | Pyre | FastAPI | Robyn |
|---------|------|---------|-------|
| GIL Watchdog | ✅ | ❌ | ❌ |
| Memory RSS monitoring | ✅ | ❌ | ❌ |
| Request counters | ✅ | ❌ | ❌ |
| Structured logging | ✅ | ✅ | ✅ |

### Developer Experience

| Feature | Pyre | FastAPI | Robyn | Notes |
|---------|------|---------|-------|-------|
| Type stubs (.pyi) | ✅ | ✅ native | ✅ | |
| TestClient | ✅ | ✅ | ❌ | |
| Env var config | ✅ | ✅ | ✅ | |
| Hot reload | ✅ `reload=True` | ✅ `--reload` | ✅ | |
| OpenAPI docs | — | ✅ | ✅ | Pyre uses MCP for AI discovery; type hints serve as docs |
| Dependency injection | — | ✅ `Depends()` | ✅ | Pyre uses `before_request` hooks for the same purpose |

### C Extension Compatibility

Python C extensions (PyO3/Rust, C/C++) use global state that isn't compatible with sub-interpreters (PEP 684). This is a CPython ecosystem limitation, not a Pyre limitation.

| Library | Sub-interp | gil=True | Why |
|---------|-----------|----------|-----|
| **pydantic** | ❌ | ✅ | pydantic-core is PyO3/Rust, global static state |
| **numpy** | ❌ | ✅ | C extension hardcodes "load once per process" |
| **pandas** | ❌ | ✅ | Depends on numpy |
| **scipy** | ❌ | ✅ | Depends on numpy |
| **orjson** | ❌ | ✅ | PyO3/Rust module |
| **sqlalchemy** | ❌ | ✅ | C extensions (greenlet, cython) |
| **pillow** | ❌ | ✅ | C extension with global state |
| **httpx** | ✅ | ✅ | Pure Python |
| **requests** | ✅ | ✅ | Pure Python |
| **json, hashlib, math** | ✅ | ✅ | stdlib, multi-interp safe |
| **asyncio, threading** | ✅ | ✅ | stdlib, per-interpreter loops |
| **dataclasses, typing** | ✅ | ✅ | Pure Python |
| **re, datetime, os** | ✅ | ✅ | stdlib |

**Rule of thumb:** if `pip show <package>` shows a `.so`/`.pyd` file, it likely needs `gil=True`. Pure Python packages always work in sub-interpreters.

**The fix is simple: add `gil=True` to routes that need C extensions.**

```python
# Fast route — sub-interpreter, 429k req/s, no C extensions needed
@app.get("/fast")
def fast(req):
    return {"hello": "world"}

# Heavy route — GIL main interpreter, full C extension support
@app.post("/analyze", model=AnalysisRequest, gil=True)
def analyze(req, data):
    import numpy as np
    import pandas as pd
    return {"mean": float(np.mean(data.values))}
```

Pyre auto-detects which routes need GIL and dispatches accordingly. Fast routes stay at 429k req/s; GIL routes get full ecosystem access. Both run concurrently in the same server.

> **When will this be fixed?** When PyO3 and numpy add PEP 684 multi-phase init support. Tracking: [PyO3#3451](https://github.com/PyO3/pyo3/issues/3451), [numpy#24003](https://github.com/numpy/numpy/issues/24003). When they do, these libraries will run at full speed in sub-interpreters — no `gil=True` needed.

> **Why no OpenAPI?** Pyre targets high-performance APIs and AI agents, not browser-based API explorers. For AI tool discovery, MCP is a more modern protocol. For human developers, Pydantic models + type stubs provide the same contract guarantees.
>
> **Why no dependency injection?** `before_request` hooks solve the same problem (auth, DB connections, shared logic) with less magic and better debuggability. DI adds framework coupling without performance benefit.

## Install

```bash
# From source (requires Rust toolchain + Python 3.12+)
git clone https://github.com/moomoo-tech/pyre.git
cd pyre
python -m venv .venv && source .venv/bin/activate
pip install maturin
maturin develop --release
```

## Demos

Three production-grade example applications. Each demonstrates a different real-world use case with multiple Pyre features working together.

```bash
# Install dependencies first
pip install pydantic numpy msgpack httpx

# AI Agent Server — MCP tools, SSE streaming, session memory
python examples/ai_agent_server.py

# Trading Data API — numpy analytics, WebSocket, Pydantic, RPC
python examples/trading_api.py

# Full-stack REST API — CRUD, cookie auth, file upload
python examples/fullstack_api.py
```

### AI Agent Server (`examples/ai_agent_server.py`)

Build MCP-compatible AI tool servers with streaming token output.

```bash
python examples/ai_agent_server.py

# Chat (simulated LLM)
curl -X POST http://127.0.0.1:8000/chat \
  -H 'Content-Type: application/json' \
  -d '{"prompt": "What is Python?", "session_id": "user1"}'

# SSE streaming (token-by-token, like ChatGPT)
curl -N http://127.0.0.1:8000/stream?prompt=hello

# MCP tool discovery (for Claude Desktop)
curl -X POST http://127.0.0.1:8000/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}'

# Session memory
curl http://127.0.0.1:8000/memory/user1
```

**Features used:** MCP Server, SSE (PyreStream), async handlers, SharedState, Pydantic, CORS

### Trading Data API (`examples/trading_api.py`)

Real-time market data with numpy analytics and WebSocket streaming.

```bash
python examples/trading_api.py

# Market quote
curl http://127.0.0.1:8000/market/AAPL

# Submit order (Pydantic validated)
curl -X POST http://127.0.0.1:8000/order \
  -H 'Content-Type: application/json' \
  -d '{"ticker": "AAPL", "side": "buy", "quantity": 100, "price": 150.5}'

# Portfolio analytics (numpy)
curl http://127.0.0.1:8000/analytics/portfolio

# RPC call from another service
python -c "
from pyreframework import PyreRPCClient
with PyreRPCClient('http://127.0.0.1:8000') as c:
    print(c.get_signals(tickers=['AAPL', 'TSLA']))
"
```

**Features used:** numpy (gil=True), Pydantic, WebSocket, SharedState, MsgPack RPC, CORS

### Full-stack REST API (`examples/fullstack_api.py`)

Complete CRUD application with authentication and file uploads.

```bash
python examples/fullstack_api.py

# Register + login
curl -X POST http://127.0.0.1:8000/auth/register \
  -H 'Content-Type: application/json' \
  -d '{"username": "alice", "email": "alice@example.com", "password": "secret123"}'

curl -c cookies.txt -X POST http://127.0.0.1:8000/auth/login \
  -H 'Content-Type: application/json' \
  -d '{"username": "alice", "password": "secret123"}'

# Create item (authenticated)
curl -b cookies.txt -X POST http://127.0.0.1:8000/items \
  -H 'Content-Type: application/json' \
  -d '{"name": "Widget", "price": 9.99, "tags": ["new"]}'

# List items (with pagination)
curl http://127.0.0.1:8000/items?page=1&per_page=10
```

**Features used:** Pydantic, Cookie auth, File upload, Redirect, SharedState as DB, CORS, structured logging

## Quick Start

### Basic API

```python
from pyreframework import Pyre, PyreResponse

app = Pyre()

@app.get("/")
def index(req):
    return {"message": "Hello from Pyre!"}

@app.get("/user/{name}")
def greet(req):
    return {"name": req.params["name"]}

@app.get("/search")
def search(req):
    return {"q": req.query_params.get("q", "")}

@app.post("/data")
def receive(req):
    return req.json()

app.run()  # http://127.0.0.1:8000
```

### Async Handlers

`def` and `async def` coexist at full speed — auto-detected, auto-routed.

```python
@app.get("/fast")
def fast(req):              # → sync pool (429k req/s)
    return "instant"

@app.get("/io")
async def io_heavy(req):    # → async pool (133k req/s)
    result = await fetch_from_database()
    return {"data": result}
```

### Pydantic Validation

```python
from pydantic import BaseModel, Field

class Order(BaseModel):
    ticker: str = Field(max_length=5)
    amount: int = Field(gt=0)
    price: float

@app.post("/order", model=Order)
def place_order(req, order: Order):
    return {"total": order.amount * order.price}
# Invalid → 422 with validation errors
```

### CORS

```python
app.enable_cors()  # Allow all origins
app.enable_cors(allow_origins=["https://example.com"], allow_credentials=True)
```

### Cookies

```python
from pyreframework.cookies import get_cookie, set_cookie, delete_cookie

@app.get("/login")
def login(req):
    return set_cookie(PyreResponse(body="ok"), "session", "abc", httponly=True)

@app.get("/me")
def me(req):
    return {"session": get_cookie(req, "session")}
```

### File Upload

```python
from pyreframework.uploads import parse_multipart

@app.post("/upload")
def upload(req):
    f = parse_multipart(req)["file"]
    return {"filename": f.filename, "size": f.size}
```

### Redirect

```python
from pyreframework import redirect

@app.get("/old")
def old(req):
    return redirect("/new")
```

### WebSocket

```python
@app.websocket("/ws")
def echo(ws):
    while True:
        msg = ws.recv()
        if msg is None: break
        ws.send(f"echo: {msg}")
```

### SSE Streaming

```python
from pyreframework import PyreStream
import threading

@app.get("/stream", gil=True)
def stream(req):
    s = PyreStream()
    def gen():
        for token in ["Hello", " ", "World"]:
            s.send_event(token)
        s.close()
    threading.Thread(target=gen).start()
    return s
```

### MCP Server (AI Agent)

```python
@app.mcp.tool(description="Add two numbers")
def add(a: int, b: int) -> int:
    return a + b
# Claude Desktop → http://localhost:8000/mcp
```

### RPC (MsgPack)

```python
@app.rpc("/rpc/compute")
def compute(data):
    return {"result": data["a"] + data["b"]}

# Client:
from pyreframework import PyreRPCClient
with PyreRPCClient("http://server:8000") as c:
    c.compute(a=3, b=5)  # → {"result": 8}
```

### Shared State

```python
app.state["key"] = "value"     # Write (any worker)
val = app.state["key"]         # Read (nanosecond, no Redis)
```

### numpy / C Extensions

```python
@app.get("/compute", gil=True)
def compute(req):
    import numpy as np
    return {"mean": float(np.mean(np.random.randn(10000)))}
```

## Configuration

```bash
PYRE_HOST=0.0.0.0 PYRE_PORT=9000 PYRE_WORKERS=16 PYRE_LOG=1 python app.py
```

## Monitoring

```bash
PYRE_METRICS=1 python app.py   # Enable GIL watchdog
```

## Testing

```python
from pyreframework.testing import TestClient

client = TestClient(app)
resp = client.get("/")
assert resp.status_code == 200
assert resp.json()["hello"] == "world"
```

## Architecture

```
Python handlers (def / async def / gil=True)
    ↓
Pyre (Rust core, 12 modules)
├── Tokio runtime (HTTP/1+2, WebSocket, SSE)
├── Sub-interpreter pool (N independent GILs)
│   ├── Sync workers (def → 429k req/s)
│   └── Async workers (async def → 133k req/s)
├── Hybrid GIL dispatch (gil=True → numpy/C extensions)
├── SharedState (DashMap, cross-worker, nanosecond)
├── GIL Watchdog (contention + hold time + queue depth)
└── Backpressure (bounded channels, 503 on overload)
```

## Sub-interpreter Safe Ecosystem

Pyre's sub-interpreters deliver 429k req/s, but C extensions (Pydantic, NumPy, Pandas) can't run in them. Instead of fighting the ecosystem, Pyre offers a **Golden Path**: modern, pure-Python alternatives that are **not just safe — they're faster**.

| Category | Traditional (needs `gil=True`) | Golden Path (sub-interp safe) |
|----------|-------------------------------|-------------------------------|
| Validation | Pydantic V2 | `msgspec` / `mashumaro` |
| Data | Pandas + NumPy | `Polars` |
| HTTP Client | requests | `httpx` |
| JSON | orjson | stdlib `json` / `msgspec` |
| Database | psycopg2 | `psycopg` v3 (pure Python) |

**The rule**: pure Python = sub-interp safe. C extensions = use `gil=True`.

### Not just safe — faster

Same endpoints, same logic. Pyre with sub-interp safe libs vs FastAPI with the traditional Pydantic stack:

| Test | FastAPI + Pydantic | Pyre + Golden Path | Speedup | Latency reduction |
|------|-------------------|-------------------|---------|-------------------|
| Health Check | 9,031 req/s | **214,714 req/s** | **23.8x** | 11.1ms → 0.38ms |
| JSON Echo | 7,602 req/s | **209,012 req/s** | **27.5x** | 13.2ms → 0.40ms |
| CPU-bound (10k moving avg) | 263 req/s | **599 req/s** | **2.3x** | 374ms → 165ms |
| Validation | 7,345 req/s | **208,439 req/s** | **28.4x** | 13.8ms → 0.41ms |

The traditional stack is single-threaded — the GIL serializes every request. Pyre runs 10 sub-interpreters in parallel, each with its own GIL. The Golden Path libraries are pure Python, so they load cleanly in every interpreter. The result: **24-28x throughput, 29-34x lower latency**.

Run the benchmark yourself: `bash benchmarks/run_comparison.sh`

> *"Pyre doesn't force you to change, but it rewards you when you do."*

Full ecosystem guide: [docs/subinterp-safe-ecosystem.md](docs/subinterp-safe-ecosystem.md)

## Limitations

Pyre's sub-interpreter architecture delivers extreme performance but comes with specific constraints. All are caused by **CPython ecosystem limitations, not Pyre design choices**, and all have clear workarounds.

### C extensions in sub-interpreters

**What:** Libraries built with PyO3 (Rust) or C/C++ extensions cannot be imported inside sub-interpreters. This includes pydantic, numpy, pandas, orjson, and most compiled packages.

**Why:** CPython's PEP 684 requires extensions to declare multi-interpreter support via `Py_MOD_PER_INTERPRETER_GIL_SUPPORTED`. Most libraries haven't done this yet. PyO3 uses global static state that conflicts with multiple interpreters.

**Workaround:** Add `gil=True` to routes that need these libraries. They run on the main interpreter with full ecosystem access while other routes run at 429k req/s on sub-interpreters.

```python
@app.get("/fast")                    # Sub-interpreter: 429k req/s
def fast(req): return "hello"

@app.post("/analyze", gil=True)      # Main interpreter: numpy works
def analyze(req):
    import numpy as np
    return {"result": float(np.mean([1,2,3]))}
```

**When fixed:** When PyO3 ([#3451](https://github.com/PyO3/pyo3/issues/3451)) and numpy ([#24003](https://github.com/numpy/numpy/issues/24003)) add PEP 684 support.

### Python 3.12+ required

**What:** Pyre requires Python 3.12 or later.

**Why:** Per-Interpreter GIL (PEP 684) was introduced in Python 3.12. This is the core technology that enables Pyre's parallelism.

**Workaround:** None. Python 3.12+ is required. Consider using [pyenv](https://github.com/pyenv/pyenv) to manage multiple Python versions.

### Build from source

**What:** Pyre must be compiled from source using Rust and Maturin. No pre-built wheels on PyPI yet.

**Why:** The project is pre-release. PyPI binary wheels for multiple platforms require CI/CD infrastructure.

**Workaround:** Install Rust (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`) and build with `maturin develop --release`.

### No OpenAPI auto-documentation

**What:** Pyre doesn't generate Swagger/OpenAPI documentation from route definitions.

**Why:** Pyre targets high-performance backends and AI agents, not browser-based API explorers. For AI tool discovery, Pyre provides native MCP (Model Context Protocol) support, which is purpose-built for AI applications. For human developers, Pydantic models and type stubs provide compile-time contract guarantees.

### Single-process only

**What:** Pyre runs as a single OS process. No multi-process mode like Gunicorn or Robyn `--fast`.

**Why:** This is by design. Sub-interpreters provide multi-core parallelism within one process, with 6.7x less memory than multi-process alternatives. SharedState works without Redis. Adding multi-process would destroy these advantages.

## Requirements

- Python 3.12+ (PEP 684 sub-interpreters)
- Rust toolchain (build from source)
- macOS or Linux

## License

Apache License 2.0 — see [LICENSE](LICENSE) for details.
