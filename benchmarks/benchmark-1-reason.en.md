# Phase 1 Benchmark Analysis: Why SkyTrade Is 2.5x Faster Than Robyn

## Test Results Recap

| Test Scenario | SkyTrade Engine | Robyn 0.82 | Multiplier |
|----------|----------------|------------|------|
| GET / (plain text) | 69,032 req/s | 27,431 req/s | 2.52x |
| GET / avg latency | 3.73ms | 10.31ms | 2.76x |
| GET /hello/bench (JSON) | 64,093 req/s | 29,113 req/s | 2.20x |
| GET /hello/bench avg latency | 4.08ms | 9.27ms | 2.27x |

Test environment: macOS ARM64, Python 3.14, Rust 1.93.1, wrk -t4 -c256 -d10s

---

## Reason 1: Lighter HTTP Stack

| | SkyTrade | Robyn |
|--|----------|-------|
| HTTP layer | Hyper (pure Rust, extremely lean) | Actix-web (feature-complete but heavier) |
| Middleware | None | Built-in OpenAPI, docs routes, logging, etc. |

Robyn automatically registers extra routes on startup:
```
Added route GET /openapi.json
Added route GET /docs
Docs hosted at http://127.0.0.1:8001/docs
```

All of this is overhead. Every request passes through Actix's middleware chain. SkyTrade is bare metal -- a request comes in, matches a route, calls the handler, and returns.

## Reason 2: Less GIL Contention

**Robyn's approach:** Every request goes through Python async scheduling (even for simple handlers), involving Python coroutine creation, event loop scheduling, and repeated GIL acquire/release cycles.

**SkyTrade's approach:**
```rust
// The entire event loop runs outside the GIL
py.detach(move || {
    rt.block_on(async { /* Tokio loop */ })
});

// The GIL is only acquired at the moment of calling the Python handler
Python::attach(|py| {
    handler.call1(py, args)  // acquire GIL -> call -> release
});
```

GIL hold time is compressed to the smallest possible granularity -- only the few microseconds while the Python handler executes. Route matching, HTTP parsing, and TCP I/O all happen in the Rust layer, completely avoiding the GIL.

## Reason 3: Faster Route Matching

| | SkyTrade | Robyn |
|--|----------|-------|
| Routing engine | matchit (radix trie-based, compile-time optimized) | Actix built-in router |
| Complexity | O(path_length), zero memory allocations | More general-purpose but heavier |

matchit is one of the fastest routing libraries in the Rust ecosystem, using compressed prefix tree matching with nearly zero memory allocations.

## Request Lifecycle Comparison

```
Robyn:
  TCP -> Actix parsing -> middleware chain -> Python async scheduling -> GIL -> coroutine creation
  -> handler execution -> coroutine completion -> GIL release -> middleware chain return -> response

SkyTrade:
  TCP -> Hyper parsing -> matchit routing -> GIL -> handler execution -> GIL release -> response
```

SkyTrade cuts out all the unnecessary layers in between.

---

## Memory Usage Comparison

Test method: Start server -> record idle RSS -> sample every 0.5s during wrk -t4 -c256 -d10s load test -> record peak and post-test RSS.

| Metric | SkyTrade Engine | Robyn 0.82 | Comparison |
|------|----------------|------------|------|
| Idle RSS | 10 MB | 35 MB | 3.5x less |
| Peak RSS (256 concurrent) | 17 MB | 46 MB | 2.7x less |
| Post-test RSS | 16 MB | 40 MB | 2.5x less |

SkyTrade idles at just 10 MB and only grows to 17 MB under full load. Robyn uses 35 MB on startup alone (Python dependencies + Actix + OpenAPI and other component overhead). This also explains why SkyTrade has lower latency -- a smaller memory footprint means better CPU cache hit rates.

---

## Areas for Further Optimization in Phase 2

| Optimization | Current State | Expected Improvement |
|--------|---------|---------|
| GIL batching strategy | Python::attach per request | Reduce GIL switching overhead |
| Zero-copy responses | String -> Bytes involves copying | Reduce memory allocations |
| SIMD-JSON | Python json.dumps | 5-10x faster |
| Multiple workers | Single Tokio runtime | Utilize multi-core CPUs |
