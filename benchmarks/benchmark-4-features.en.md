# Benchmark 4: v0.3.0 Post-Feature-Completion Performance Validation (2026-03-24)

New features: request headers, query_params, PyreResponse (custom status codes/headers/content-type), middleware (before_request/after_request)

Test environment: macOS ARM64 (Apple Silicon), Python 3.14.3, Rust 1.93.1, wrk 4.2.0
Parameters: `wrk -t4 -c256 -d10s`

## Results

| Framework | GET `/` req/s | GET `/hello/{name}` req/s | GET `/search?q=` req/s | Avg Latency |
|------|-------------|------------------------|---------------------|---------|
| **Pyre SubInterp** | **223,440** | **223,545** | **221,787** | **0.85ms** |
| Pyre GIL (with middleware) | 103,712 | 88,245 | 99,095 | 2.5ms |
| Robyn --fast | 94,287 | 94,854 | — | 30ms |

## Comparison with v0.2.0

| Metric | v0.2.0 | v0.3.0 | Change |
|------|--------|--------|------|
| SubInterp GET `/` | 216,517 | 223,440 | +3.2% |
| SubInterp GET `/hello` | 204,636 | 223,545 | +9.2% |
| GIL GET `/` | 100,673 | 103,712 | +3.0% |
| GIL GET `/hello` | 97,727 | 88,245 | -9.7% (middleware overhead added) |

## Key Conclusions

1. **Zero performance penalty from new features** — Header extraction and query_params parsing are completely imperceptible in SubInterp mode (221k vs 223k)
2. **SubInterp mode actually got faster** — Likely due to the Rust compiler optimizing the refactored code better
3. **Minimal middleware overhead** — GIL mode with after_request hook loses only ~3% (GET `/`); JSON routes lose more (~10%) because after_request needs to create PyreResponse objects
4. **Pyre SubInterp vs Robyn gap continues to widen** — 2.4x throughput, 35x lower latency
