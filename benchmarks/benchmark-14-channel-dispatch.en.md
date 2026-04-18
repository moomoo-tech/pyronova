# Benchmark 14: Channel Dispatch Latency — GIL Mode vs Sub-interp Mode (2026-04-17)

## Purpose

Quantify the crossbeam channel + tokio oneshot cross-thread dispatch overhead in Pyre's sub-interpreter mode. Determine whether channel round-trip is a performance bottleneck, and evaluate the need for lock-free ring buffers or arena pointer passing.

## Background

In sub-interp mode, each request lifecycle is:

```
Tokio task → crossbeam::bounded.try_send(WorkRequest) → OS thread recv()
  → Python handler execution → oneshot::Sender.send(Response) → Tokio task wakeup
```

In theory, cross-thread queue contention and wakeup latency could become bottlenecks at extreme concurrency. This test isolates dispatch overhead by comparing GIL mode (no channel, direct call) against sub-interp mode (full channel round-trip).

## Environment

| Item | Value |
|------|-------|
| CPU | Apple M4 (10 cores) |
| OS | macOS Darwin 25.4.0 |
| Python | 3.14.3 (per-interpreter GIL) |
| Rust | stable |
| Pyre | v1.3.0 |
| Channel | crossbeam-channel 0.5 (bounded, capacity = workers * 128) |
| Response | tokio::sync::oneshot |
| Tool | wrk 4.2.0 |
| Handler | `return "ok"` (minimal plaintext, isolates handler logic) |

### Dispatch Architecture

| Mode | Path | Channel Overhead |
|------|------|-----------------|
| GIL mode | Tokio task → `Python::attach()` → handler → direct return | None |
| Sub-interp | Tokio task → `crossbeam.try_send()` → worker `recv()` → handler → `oneshot.send()` → Tokio wakeup | Full round-trip |

## Results

### Throughput (req/s)

| Concurrency | GIL Mode | Sub-interp Mode | Factor |
|-------------|---------|-----------------|--------|
| c64 (2t) | 81,479 | **185,548** | **2.3x** |
| c256 (4t) | 71,340 | **193,179** | **2.7x** |
| c512 (4t) | 53,262 | **183,238** | **3.4x** |

### Average Latency

| Concurrency | GIL Mode | Sub-interp Mode | Reduction |
|-------------|---------|-----------------|-----------|
| c64 | 1.00ms | **337us** | **-66%** |
| c256 | 4.63ms | **1.14ms** | **-75%** |
| c512 | 12.32ms | **2.34ms** | **-81%** |

### Throughput Comparison (c256)

```
GIL mode      ███████████████████                          71,340 req/s
Sub-interp    ████████████████████████████████████████████████████ 193,179 req/s  (+171%)
```

### Latency Comparison (c256)

```
GIL mode      ████████████████████  4.63ms
Sub-interp    █████                 1.14ms  (-75%)
```

### Scalability Curve

```
Throughput (req/s)
200k ┤                    ●─────────────●─────────────●  Sub-interp
     │
150k ┤
     │
100k ┤
     │  ●
 80k ┤  │╲
     │     ╲
 60k ┤      ╲●
     │        ╲
 40k ┤         ╲●  GIL mode
     │
     └──────────┬─────────────┬─────────────┬──
              c64           c256          c512
```

## Key Findings

### 1. Channel Round-Trip Is Not a Bottleneck — It's an Accelerator

Sub-interp mode significantly outperforms GIL mode at all concurrency levels. Despite adding crossbeam channel + oneshot dispatch overhead, throughput improves 2.3-3.4x. This proves GIL contention is the real bottleneck, not cross-thread communication.

### 2. Sub-interp Throughput Stays Flat Under High Concurrency

From c64 to c512, sub-interp throughput only drops from 186k to 183k (-1.6%). GIL mode drops from 81k to 53k (-35%). This demonstrates that crossbeam's backpressure management (bounded queue + `try_send`) works well with no significant lock contention.

### 3. crossbeam-channel Is Already Lock-Free

crossbeam-channel 0.5 is based on Dmitry Vyukov's MPMC algorithm and takes the lock-free hot path under high load (no futex). Replacing it with a custom ring buffer would yield single-digit nanosecond improvements, while Python handler execution operates at the microsecond scale.

### 4. Oneshot Channel Creation Overhead Is Negligible

`tokio::sync::oneshot::channel()` costs ~100ns to create. At 186k QPS, total cost is ~18ms/s, or < 2% of CPU time.

### 5. WorkRequest Data Transfer Is Near-Optimal

- `body: Bytes` — reference-counted zero-copy (from hyper)
- `headers` — `LazyHeaders::Raw(HeaderMap)` passes raw hyper header map directly; only converts to HashMap when Python accesses `req.headers` (`OnceLock`)
- Largest allocation cost comes from `method`, `path`, `client_ip` as `String`, but these are short strings (< 64 bytes) with nanosecond-scale allocation

## Optimization Assessment

| Optimization | Theoretical Gain | Practical Value | Verdict |
|-------------|-----------------|----------------|---------|
| Lock-free Ring Buffer replacing crossbeam | ~10ns/op | ~2ms/s total savings at 186k QPS | Not worth it |
| Arena pointer passing (replacing oneshot) | ~100ns/op | < 2% of total, adds unsafe complexity | Not worth it |
| Batch dispatching (bulk consume) | Fewer wakeups | crossbeam already avoids futex under load | Not worth it |
| Reduce Python-side PyObject creation | Reduces handler execution time | The actual ceiling | **Worth it** |

## Conclusion

**The crossbeam channel + tokio oneshot dispatch architecture is near-optimal.** With a minimal handler, sub-interp mode achieves 193k req/s — 2.7x faster than GIL mode. Channel dispatch overhead is completely dominated by the parallelization gains from eliminating GIL contention.

The performance ceiling is Python handler execution speed, not the Rust dispatch layer. Next optimization targets should focus on:
1. Reducing PyObject creation in the handler call path
2. Caching compiled handler bytecode
3. Optimizing the FFI call chain in `call_handler`
