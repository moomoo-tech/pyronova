# Pyre

**High-performance Python web framework powered by Rust.**

220,000 req/s. 67 MB memory. Sub-interpreter parallelism. One process.

```python
from skytrade import Pyre

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

## Performance

Benchmarked on Apple Silicon (M-series), Python 3.14, wrk -t4 -c256 -d10s.

| Scenario | Pyre | FastAPI | Robyn | Pyre vs FastAPI |
|----------|------|---------|-------|----------------|
| Hello World | **220,000** | 15,000 | 87,000 | **14.7x** |
| JSON response | **220,000** | 12,000 | 86,000 | **18.3x** |
| I/O (sleep 1ms) | **133,000** | 50,000 | 93,000 | **2.7x** |
| CPU (fib 10) | **212,000** | 8,000 | 81,000 | **26.5x** |
| JSON parse 7KB | **99,000** | 6,000 | 57,000 | **16.5x** |

| Resource | Pyre | FastAPI | Robyn |
|----------|------|---------|-------|
| Memory (10 workers) | **119 MB** | ~200 MB | 447 MB |
| P99 latency | **4.2 ms** | ~20 ms | 262 ms |
| Processes | **1** | 1+ Gunicorn | 22 |

## Why is Pyre fast?

Pyre uses **Per-Interpreter GIL** (PEP 684) — each worker has its own independent Python interpreter with its own GIL. True multi-core parallelism in a single process, without the memory overhead of multi-processing.

```
Traditional (FastAPI/Robyn):
  1 process × 1 GIL = all cores fight for one lock

Pyre:
  1 process × 10 sub-interpreters × 10 independent GILs = true parallelism
```

## Feature Comparison

### Routing & Request/Response

| Feature | Pyre | FastAPI | Robyn |
|---------|------|---------|-------|
| Decorator routing | ✅ | ✅ | ✅ |
| Path params `/hello/{name}` | ✅ | ✅ | ✅ |
| Query params | ✅ | ✅ | ✅ |
| JSON parsing | ✅ | ✅ | ✅ |
| Pydantic validation | ✅ `model=` | ✅ native | ❌ |
| File upload (multipart) | ✅ | ✅ | ✅ |
| Cookie read/write | ✅ | ✅ | ✅ |
| Redirect | ✅ | ✅ | ✅ |
| Custom status/headers | ✅ | ✅ | ✅ |
| Static files | ✅ | ✅ | ✅ |

### Protocols

| Feature | Pyre | FastAPI | Robyn |
|---------|------|---------|-------|
| HTTP/1.1 | ✅ | ✅ | ✅ |
| HTTP/2 | ✅ | ✅ (uvicorn) | ✅ |
| WebSocket (text+binary) | ✅ | ✅ | ✅ |
| SSE streaming | ✅ | ✅ | ❌ |

### Middleware & Security

| Feature | Pyre | FastAPI | Robyn |
|---------|------|---------|-------|
| before/after hooks | ✅ | ✅ middleware | ✅ |
| CORS | ✅ built-in | ✅ | ✅ |
| Body size limit | ✅ 10MB | ✅ | ❌ |
| Backpressure (503) | ✅ | ❌ | ❌ |
| Path traversal protection | ✅ | ✅ | ❌ |
| Worker panic protection | ✅ catch_unwind | N/A | N/A |

### AI & Microservices

| Feature | Pyre | FastAPI | Robyn |
|---------|------|---------|-------|
| MCP Server (AI tools) | ✅ native | ❌ | ✅ |
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
| Type stubs (.pyi) | ✅ | ✅ native | ❌ | |
| TestClient | ✅ | ✅ | ✅ | |
| Env var config | ✅ | ✅ | ✅ | |
| Hot reload | ✅ `reload=True` | ✅ `--reload` | ✅ | |
| OpenAPI docs | — | ✅ | ❌ | Pyre uses MCP for AI discovery; type hints serve as docs |
| Dependency injection | — | ✅ `Depends()` | ❌ | Pyre uses `before_request` hooks for the same purpose |

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

## Quick Start

### Basic API

```python
from skytrade import Pyre, SkyResponse

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
def fast(req):              # → sync pool (220k req/s)
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
from skytrade.cookies import get_cookie, set_cookie, delete_cookie

@app.get("/login")
def login(req):
    return set_cookie(SkyResponse(body="ok"), "session", "abc", httponly=True)

@app.get("/me")
def me(req):
    return {"session": get_cookie(req, "session")}
```

### File Upload

```python
from skytrade.uploads import parse_multipart

@app.post("/upload")
def upload(req):
    f = parse_multipart(req)["file"]
    return {"filename": f.filename, "size": f.size}
```

### Redirect

```python
from skytrade import redirect

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
from skytrade import SkyStream
import threading

@app.get("/stream", gil=True)
def stream(req):
    s = SkyStream()
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
from skytrade import PyreRPCClient
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
from skytrade.testing import TestClient

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
│   ├── Sync workers (def → 220k req/s)
│   └── Async workers (async def → 133k req/s)
├── Hybrid GIL dispatch (gil=True → numpy/C extensions)
├── SharedState (DashMap, cross-worker, nanosecond)
├── GIL Watchdog (contention + hold time + queue depth)
└── Backpressure (bounded channels, 503 on overload)
```

## Requirements

- Python 3.12+ (PEP 684 sub-interpreters)
- Rust toolchain (build from source)
- macOS or Linux

## License

MIT
