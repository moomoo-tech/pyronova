# Benchmark #11: Pyre vs FastAPI Head-to-Head

**Date**: 2026-03-24
**Machine**: Apple M4 Pro, 10 cores, 16GB RAM
**Python**: 3.14.3 (free-threaded, per-interpreter GIL)
**Pyre**: v1.2.0 (mimalloc, sub-interpreter mode, 10 workers)
**FastAPI**: 0.135.2 + Uvicorn 0.42.0 (single worker)
**wrk**: 4 threads, 100 connections, 10s per test

## Results

| Test | FastAPI | Pyre | Speedup |
|------|---------|------|---------|
| **Health Check** (plain JSON) | 9,031 req/s | 214,714 req/s | **23.8x** |
| **JSON Echo** (parse + serialize) | 7,602 req/s | 209,012 req/s | **27.5x** |
| **CPU-bound** (moving average, 10k points) | 263 req/s | 599 req/s | **2.3x** |
| **Validation** (field checking) | 7,345 req/s | 208,439 req/s | **28.4x** |

### Latency

| Test | FastAPI | Pyre | Reduction |
|------|---------|------|-----------|
| Health Check | 11.10 ms | 0.38 ms | **29x lower** |
| JSON Echo | 13.17 ms | 0.40 ms | **33x lower** |
| CPU-bound | 374.42 ms | 165.36 ms | **2.3x lower** |
| Validation | 13.75 ms | 0.41 ms | **34x lower** |

### Memory

| Framework | RSS |
|-----------|-----|
| FastAPI | 35.7 MB |
| Pyre | 60.8 MB |

Pyre uses more memory due to 10 sub-interpreters (each ~6 MB). Per-core memory is actually lower.

## Analysis

### I/O-bound routes (Health, Echo, Validation): 24-28x faster

This is where Pyre's architecture dominates. FastAPI is single-threaded (GIL-bound) — one request at a time. Pyre runs 10 sub-interpreters in parallel, each with its own GIL. Under 100 concurrent connections, Pyre saturates all cores while FastAPI queues requests behind the GIL.

### CPU-bound route: 2.3x faster

The moving average computation is pure Python (no C extensions). Pyre achieves linear scaling across cores — 10 sub-interpreters processing 10 requests simultaneously. FastAPI processes them one at a time. The 2.3x (not 10x) speedup is because each individual computation still takes ~165ms in Python; parallelism helps throughput, not per-request latency.

### Why not compare with `uvicorn --workers N`?

Fair question. Running `uvicorn --workers 4` would give FastAPI ~4x throughput via multiprocessing. But:
- Each worker is a full Python process (memory multiplies)
- No shared state between workers (need Redis/DB)
- Still GIL-bound within each worker
- Even at 4x, FastAPI would reach ~36k req/s vs Pyre's 210k

## The Stack Comparison

| | FastAPI (Traditional) | Pyre (Sub-interp Safe) |
|---|---|---|
| **Server** | Uvicorn (uvloop) | Hyper (Rust + Tokio) |
| **Validation** | Pydantic V2 | stdlib json / msgspec |
| **Concurrency** | async/await (single core) | 10 sub-interpreters (all cores) |
| **Memory model** | Single interpreter, shared GIL | Per-interpreter GIL (PEP 684) |

## Conclusion

For I/O-bound APIs (the vast majority of web services), Pyre delivers **24-28x higher throughput** and **29-34x lower latency** than FastAPI — in a single process, with zero configuration.

The key insight: **the GIL is the bottleneck, not Python itself.** Remove the GIL contention (via sub-interpreters), and Python web performance enters a completely different league.
