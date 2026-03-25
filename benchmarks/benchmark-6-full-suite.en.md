# Benchmark 6: v0.4.0 Full Stress Test — spawn_blocking + Backpressure + TCP_NODELAY (2026-03-24)

Optimizations: spawn_blocking to isolate GIL calls, bounded channel backpressure (503), TCP_NODELAY,
orjson auto-detection, 10MB body limit, async def detection, type stubs

Test environment: macOS ARM64 (Apple Silicon), Python 3.14.3, Rust 1.93.1, wrk 4.2.0
Parameters: warmup wrk -t2 -c50 -d3s, main run wrk -t4 -c256 -d10s

## Full Results

| Scenario | Pyre SubInterp | Pyre Hybrid | Pyre GIL | Robyn --fast | Winner |
|------|----------------|-------------|----------|-------------|------|
| Hello World | 211,019 | **216,674** | 77,853 | 81,251 | Pyre **2.7x** |
| JSON small (3 fields) | **212,521** | 210,479 | 73,777 | 80,672 | Pyre **2.6x** |
| JSON medium (100 users) | 63,649 | **67,817** | 13,293 | 42,775 | Pyre **1.6x** |
| JSON large (500 records) | **5,063** | 4,835 | 1,917 | 4,467 | Pyre **13%** |
| fib(10) | 205,092 | **205,694** | 58,876 | 74,444 | Pyre **2.8x** |
| fib(20) | 10,830 | **11,065** | 1,899 | 8,664 | Pyre **28%** |
| fib(30) | **91** | 91 | 1 | 79 | Pyre **15%** |
| Pure Python sum(10k) | **74,894** | 74,362 | 18,036 | 43,836 | Pyre **1.7x** |
| sleep(1ms) | 7,867 | 7,905 | **46,704** | **77,967** | **Robyn wins** |
| Parse 41B JSON | 203,262 | **208,249** | 60,514 | 75,277 | Pyre **2.8x** |
| Parse 7KB JSON | 90,867 | **94,942** | 19,873 | 50,520 | Pyre **1.9x** |
| Parse 93KB JSON | **9,958** | 9,750 | 1,847 | 7,345 | Pyre **36%** |
| numpy mean(10k) | — | 8,507 | 8,290 | **31,261** | **Robyn wins** |
| numpy SVD 100x100 | — | 3,993 | 3,940 | **5,079** | **Robyn wins** |

**Total: Pyre wins 10/14 scenarios, Robyn wins 3/14, tied 1/14**

## spawn_blocking Trade-off Analysis

| Metric | Before optimization (Round 1) | After optimization (Round 2) | Change | Reason |
|------|-----------------|-----------------|------|------|
| GIL Hello World | 119,049 | 77,853 | **-34%** | One extra thread switch per request |
| GIL sleep(1ms) | 7,354 | 46,704 | **+535%** | No longer blocks Tokio workers |
| GIL numpy mean | 8,419 | 8,290 | -1.5% | numpy itself is CPU bound |
| SubInterp Hello | 219,210 | 211,019 | -3.7% | Normal fluctuation |
| Hybrid Hello | 217,610 | 216,674 | -0.4% | Unaffected |

**Conclusion**: GIL mode pure throughput drops 34% (thread switching cost), but I/O concurrency improves 535%.
In real-world web applications, handlers inevitably involve I/O (database, network, files), so spawn_blocking is a net gain.
SubInterp/Hybrid modes are completely unaffected (they use the channel pool, bypassing spawn_blocking).

## Pyre vs Robyn Head-to-Head Analysis

### Scenarios Where Pyre Dominates Robyn (2-3x)

| Scenario | Pyre Advantage | Reason |
|------|----------|------|
| Hello World | 2.7x | Sub-interpreters enable true parallelism, zero GIL contention |
| fib(10) light CPU | 2.8x | 10 independent GILs computing in parallel |
| JSON parse 41B | 2.8x | Rust routing + sub-interpreter parallelism |
| sum(10k) pure Python | 1.7x | Multiple interpreters eliminate GIL bottleneck |

### Scenarios Where Robyn Wins

| Scenario | Robyn Advantage | Reason | Pyre Improvement Direction |
|------|----------|------|---------------|
| sleep(1ms) I/O | 10x | Robyn async + multi-process; Pyre sync handler | Support async handlers |
| numpy mean | 3.7x | Robyn multi-process naturally parallelizes numpy | Multi-process mode or ProcessPool |
| numpy SVD | 1.3x | Same as above | Same as above |

### Close/Tied Scenarios

| Scenario | Gap | Analysis |
|------|------|------|
| JSON large (500 records) | Pyre +13% | Bottleneck is Python JSON serialization; both sides are roughly equal |
| fib(30) heavy CPU | Pyre +15% | Fully CPU bound; gap comes from number of parallel workers |

## Architecture Comparison: Pyre vs Robyn vs Theoretical Ceiling

```
Dimension       Pyre (current)           Robyn                Next Breakthrough
─────────────────────────────────────────────────────────────────────────
I/O model       Tokio (epoll)           Tokio (epoll)        io_uring (monoio)
Parallelism     Sub-interpreter (OWN_GIL) Multi-process      ← Pyre's unique advantage
JSON serial.    orjson auto-detection    Python json          SIMD-JSON (Rust)
GIL strategy    Per-Interpreter GIL     Frequent acquire/release  ← Pyre's unique advantage
WebSocket       tokio-tungstenite       actix-ws             On par
HTTP protocol   HTTP/1.1                HTTP/1.1             HTTP/2, HTTP/3
Route matching  matchit (compile-time)  actix-router         On par
Backpressure    bounded channel + 503   None (potential OOM) ← Pyre's advantage
Security        10MB body + path traversal defense  Unknown  ← Pyre's advantage
```
