# The Sub-interpreter Safe Ecosystem

Pyre achieves 220,000+ req/s by running Python handlers across multiple sub-interpreters, each with its own GIL (PEP 684). This means **true multi-core parallelism** in a single process — but it also means some traditional Python libraries won't work in sub-interpreter mode.

This guide maps out the **Golden Path**: libraries that are verified safe for sub-interpreters, delivering maximum performance with zero compatibility headaches.

> **Philosophy**: Pyre doesn't force you to change, but it rewards you when you do.

## Not Just Safe — Faster

Switching to sub-interpreter safe libraries isn't a compromise. It's an upgrade. Benchmark results (Apple M4 Pro, 10 cores, wrk -t4 -c100 -d10s):

| Test | FastAPI + Pydantic | Pyre + Golden Path | Speedup |
|------|-------------------|-------------------|---------|
| Health Check (plain JSON) | 9,031 req/s | **214,714 req/s** | **23.8x** |
| JSON Echo (parse + serialize) | 7,602 req/s | **209,012 req/s** | **27.5x** |
| CPU-bound (10k moving avg) | 263 req/s | **599 req/s** | **2.3x** |
| Validation (field checking) | 7,345 req/s | **208,439 req/s** | **28.4x** |

| Metric | FastAPI + Pydantic | Pyre + Golden Path |
|--------|-------------------|-------------------|
| Avg latency (Health) | 11.10 ms | **0.38 ms** (29x lower) |
| Avg latency (Echo) | 13.17 ms | **0.40 ms** (33x lower) |
| Avg latency (Validate) | 13.75 ms | **0.41 ms** (34x lower) |

The traditional stack (FastAPI + Pydantic) is single-threaded — the GIL serializes all requests. Pyre with sub-interp safe libs runs 10 interpreters in parallel, each at full CPU speed. The result: **24-28x throughput** and **29-34x lower latency** for typical API workloads.

Run the benchmark yourself: `bash benchmarks/run_comparison.sh`

## Quick Reference

| Category | Traditional (GIL-bound) | Golden Path (Sub-interp Safe) | Why it matters |
|----------|------------------------|-------------------------------|----------------|
| Validation | Pydantic V2 | `msgspec` / `mashumaro` | Pydantic-core is a Rust C extension — crashes in sub-interpreters |
| Data Processing | Pandas + NumPy | `Polars` | Pure Rust internals, Arrow memory model, zero-copy potential |
| HTTP Client | `requests` | `httpx` (sync mode) | Pure Python, no C dependencies |
| JSON | `orjson` / `ujson` | `json` (stdlib) or `msgspec` | C extensions unsafe; stdlib json is sub-interp safe |
| Database | `psycopg2` | `psycopg` (v3, pure Python) | psycopg2 is C; v3 has pure-Python mode |
| ORM | SQLAlchemy + C driver | SQLAlchemy + pure-Python driver | Use `psycopg[binary]` for GIL routes, `psycopg` for sub-interp |
| Serialization | `pickle` | `msgspec` / `msgpack` | msgspec is pure Python with optional C speedups |
| Templating | Jinja2 | Jinja2 ✅ | Pure Python — works in sub-interpreters |
| Crypto | `cryptography` | `hashlib` (stdlib) | OpenSSL C bindings unsafe; stdlib hashlib is safe |

## The Rule of Thumb

**Pure Python = Safe.** If a library is written entirely in Python (no C extensions, no `.so`/`.pyd` files), it will work in sub-interpreters.

**C extensions = Use `gil=True`.** Libraries with C extensions (NumPy, Pydantic V2, SQLAlchemy C drivers) must run on the main interpreter. Pyre makes this easy:

```python
@app.get("/analytics", gil=True)  # Runs on main interpreter
def analytics(req):
    import pandas as pd  # Safe — running with GIL
    df = pd.read_csv("data.csv")
    return {"mean": df["price"].mean()}
```

## Deep Dives

### 1. Validation: msgspec over Pydantic

[Pydantic V2](https://docs.pydantic.dev/) is the most popular validation library, but its core is compiled Rust (`pydantic-core`). It cannot be loaded in multiple sub-interpreters simultaneously.

**Golden Path: [msgspec](https://jcristharris.com/msgspec/)**

```python
import msgspec

class Order(msgspec.Struct):
    symbol: str
    quantity: int
    price: float

@app.post("/order")
def create_order(req):
    order = msgspec.json.decode(req.body, type=Order)
    return {"accepted": True, "symbol": order.symbol}
```

Why msgspec:
- **Faster than Pydantic** even in single-interpreter mode (5-10x for JSON decoding)
- Pure Python with optional C speedups — safe in sub-interpreters
- Built-in JSON, MessagePack, TOML encoding/decoding
- Struct validation at decode time (no separate validation step)

**Alternative: [mashumaro](https://github.com/Fatal1ty/mashumaro)** — dataclass-based, similar performance, more familiar API for dataclass users.

### 2. Data Processing: Polars over Pandas

[Pandas](https://pandas.pydata.org/) depends heavily on NumPy's C-API with global interpreter state. It will crash in sub-interpreters.

**Golden Path: [Polars](https://pola.rs/)**

```python
@app.get("/report")
def generate_report(req):
    import polars as pl
    df = pl.read_csv("trades.csv")
    result = (
        df.group_by("symbol")
        .agg(pl.col("price").mean().alias("avg_price"))
        .sort("avg_price", descending=True)
        .head(10)
    )
    return result.to_dicts()
```

Why Polars is the perfect match for Pyre:
- **Same DNA**: Polars is written in Rust, just like Pyre's core
- **Apache Arrow memory model**: columnar, cache-friendly, zero-copy potential
- **No GIL dependency**: Polars releases the GIL for all heavy operations
- **Lazy evaluation**: query optimization before execution
- **3-10x faster than Pandas** for most operations

> **Future**: We're exploring zero-copy Arrow transfer between Pyre's Rust layer and Polars — eliminating serialization entirely for data-heavy endpoints.

### 3. HTTP Client: httpx

`requests` is pure Python and technically safe, but `httpx` offers a more modern API with connection pooling that works well in isolated interpreters.

```python
import httpx

@app.get("/proxy")
def proxy_request(req):
    resp = httpx.get("https://api.example.com/data")
    return resp.json()
```

### 4. Database: psycopg v3 (Pure Python mode)

```python
import psycopg  # v3, NOT psycopg2

@app.get("/users")
def list_users(req):
    with psycopg.connect("postgresql://localhost/mydb") as conn:
        rows = conn.execute("SELECT id, name FROM users").fetchall()
        return [{"id": r[0], "name": r[1]} for r in rows]
```

For connection pooling across sub-interpreters, use Pyre's `SharedState` to coordinate, or use an external pool like PgBouncer.

## Hybrid Architecture: Best of Both Worlds

You don't have to choose one mode. Pyre's hybrid dispatch lets you mix sub-interpreter routes (fast, parallel) with GIL routes (full ecosystem access):

```python
from pyreframework import Pyre

app = Pyre()

# ⚡ Sub-interpreter: 220k req/s, pure Python only
@app.get("/health")
def health(req):
    return {"status": "ok"}

@app.post("/validate")
def validate(req):
    import msgspec
    data = msgspec.json.decode(req.body, type=MyStruct)
    return {"valid": True}

# 🔒 GIL route: full ecosystem, single-threaded
@app.post("/ml-predict", gil=True)
def predict(req):
    import numpy as np
    import pandas as pd
    # Full access to C extensions
    return {"prediction": model.predict(features)}
```

**Architecture rule**: Put your hot paths (high-QPS endpoints) on sub-interpreters with safe libraries. Put your cold paths (admin, ML inference, analytics) on GIL routes with any library you want.

## Checking if a Library is Sub-interpreter Safe

```bash
# Quick check: does the library contain C extensions?
python -c "
import importlib, pathlib
pkg = 'msgspec'  # change this
spec = importlib.util.find_spec(pkg)
if spec and spec.submodule_search_locations:
    for loc in spec.submodule_search_locations:
        exts = list(pathlib.Path(loc).rglob('*.so')) + list(pathlib.Path(loc).rglob('*.pyd'))
        if exts:
            print(f'⚠️  {pkg} has C extensions — use gil=True')
            for e in exts: print(f'   {e.name}')
        else:
            print(f'✅ {pkg} appears to be pure Python — sub-interp safe')
"
```

## Community-Verified Libraries

✅ **Verified Safe** (pure Python, tested with Pyre sub-interpreters):
- `msgspec`, `mashumaro` — validation/serialization
- `httpx` — HTTP client
- `Jinja2` — templating
- `python-multipart` — form parsing
- `PyJWT` — JSON Web Tokens (pure Python mode)
- `itsdangerous` — signed tokens
- `python-dotenv` — env loading
- Standard library: `json`, `hashlib`, `hmac`, `uuid`, `datetime`, `re`, `sqlite3`

⚠️ **Requires `gil=True`** (C extensions):
- `pydantic` (v2) — pydantic-core is Rust/C
- `numpy` — C extension
- `pandas` — depends on numpy
- `scipy` — C/Fortran extensions
- `orjson` — C extension
- `uvloop` — C extension (not relevant for Pyre)
- `psycopg2` — C extension
- `cryptography` — OpenSSL bindings
- `lxml` — libxml2 bindings

❓ **Untested** (community reports welcome):
- `polars` — Rust-based, theoretically safe but needs verification
- `sqlmodel` — depends on SQLAlchemy + driver
- `aiohttp` — mixed C/Python

---

*This document is a living guide. If you've tested a library with Pyre sub-interpreters, please open an issue or PR to update this list.*
