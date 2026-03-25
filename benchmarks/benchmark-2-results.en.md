# Phase 2 + Phase 4 Benchmark Results (2026-03-23)

Test environment: macOS ARM64 (Apple Silicon), Python 3.14.3, Rust 1.93.1, wrk 4.2.0
Parameters: `wrk -t4 -c256 -d10s`

## Full Comparison of Four Modes

### Throughput (req/s)

| Mode | GET / | GET /hello/bench | Notes |
|------|-------|-----------------|------|
| **Pyre SubInterp** | **213,759** | **198,441** | 10 sub-interpreters, each with independent GIL |
| Pyre GIL | 95,926 | 91,722 | Standard Python 3.14, single GIL |
| Pyre NO-GIL | 95,374 | 86,182 | Free-threaded Python 3.14t |
| Robyn --fast | 71,032 | 70,440 | 22-process multi-worker mode |

### Latency (avg)

| Mode | GET / | GET /hello/bench |
|------|-------|-----------------|
| **Pyre SubInterp** | **0.91ms** | **1.06ms** |
| Pyre GIL | 2.85ms | 2.86ms |
| Pyre NO-GIL | 2.83ms | 3.34ms |
| Robyn --fast | 22.23ms | 29.13ms |

### Memory Usage (RSS)

| Mode | Idle | Peak (256 concurrent) | Post-test |
|------|------|---------------|--------|
| Pyre GIL | 10 MB | 16 MB | 16 MB |
| Pyre NO-GIL | 18 MB | 29 MB | 29 MB |
| **Pyre SubInterp** | **52 MB** | **67 MB** | **67 MB** |
| Robyn --fast | 437 MB | 451 MB | 432 MB |

## Key Comparison: Pyre SubInterp vs Robyn --fast

| Metric | Pyre SubInterp | Robyn --fast | Multiplier |
|------|---------------|-------------|------|
| GET / throughput | 213,759 req/s | 71,032 req/s | **3.0x** |
| GET /hello throughput | 198,441 req/s | 70,440 req/s | **2.8x** |
| GET / latency | 0.91ms | 22.23ms | **24x lower** |
| Peak memory | 67 MB | 451 MB | **6.7x less** |

## Analysis

### Why Sub-Interpreter Mode Is the Fastest
- 10 sub-interpreters each hold an independent GIL (OWN_GIL), enabling true parallel execution of Python handlers
- Within the same process, they share heap/code pages, yielding far better memory efficiency than multi-process approaches
- Each sub-interpreter adds ~5 MB of overhead, while each Robyn process adds ~20 MB

### Why NO-GIL Is Not as Fast as Expected
- Free-threaded Python's atomic operations and reference counting overhead offset some of the parallelism gains
- For simple handlers (a few microseconds), GIL contention itself is not the primary bottleneck
- Sub-interpreters are fully isolated with no shared state contention, making them actually faster

### Memory Efficiency
- Pyre SubInterp at 67 MB peak delivers 213k req/s = **3,184 req/s/MB**
- Robyn --fast at 451 MB peak delivers 71k req/s = **157 req/s/MB**
- **Pyre's memory efficiency is 20x that of Robyn**
