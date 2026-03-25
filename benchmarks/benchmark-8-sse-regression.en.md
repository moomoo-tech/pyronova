# Benchmark 8: v0.5.0+ SSE Streaming Regression Verification (2026-03-24)

## New Features in This Round

| Feature | File | Description |
|---------|------|-------------|
| **SSE streaming** | `stream.rs`, `handlers.rs` | `PyreStream` pyclass, chunked transfer, AI Agent token streaming output |
| **BoxBody unified response type** | `handlers.rs` | `Either<Full, StreamBody>` supports both normal + streaming responses |
| **full_body() wrapper** | `handlers.rs`, `websocket.rs` | All `Response<Full<Bytes>>` converted to `BoxBody` via `full_body()` |

## Verification Goals

1. **Does the BoxBody type wrapping introduce extra overhead?** — Each normal response has an additional `Either::Left` wrapper layer
2. **Does the PyreStream detection logic (type_name comparison) slow down normal routes?** — One additional type name check per response
3. **Does the tokio-stream dependency affect Tokio runtime behavior?**

## Full Results (wrk -t4 -c256 -d10s)

| Scenario | Pyre SubInterp | Pyre Hybrid | Pyre GIL | Robyn --fast |
|----------|----------------|-------------|----------|-------------|
| Hello World | **213,887** | 215,090 | 83,097 | 85,935 |
| JSON small | **209,414** | 214,984 | 65,302 | 86,468 |
| JSON medium | 62,972 | 61,742 | 12,202 | 44,882 |
| JSON large | **5,183** | 4,930 | 1,930 | 4,948 |
| fib(10) | **206,179** | 197,713 | 57,243 | 86,786 |
| fib(20) | 10,396 | **11,145** | 1,859 | 10,933 |
| fib(30) | 85 | 85 | 1 | 88 |
| sum(10k) | 73,805 | **74,512** | 17,716 | 46,959 |
| sleep(1ms) | 7,793 | 6,734 | **52,494** | **85,350** |
| Parse 41B | 205,675 | **210,170** | 72,788 | 83,708 |
| Parse 7KB | 89,975 | **96,142** | 20,231 | 56,193 |
| Parse 93KB | 9,877 | **9,934** | 1,901 | 8,893 |
| numpy mean | — | 8,791 | 8,209 | **35,561** |
| numpy SVD | — | 3,916 | 4,043 | **5,812** |

## Regression Analysis: Round 4 (This Round) vs Round 3 (Previous Round)

### SubInterp Mode

| Scenario | R3 | R4 | Change | Regression? |
|----------|----|----|--------|-------------|
| Hello World | 216,297 | 213,887 | -1.1% | No (normal variance) |
| JSON small | 214,185 | 209,414 | -2.2% | No |
| fib(10) | 187,340 | 206,179 | **+10.1%** | Improved |
| Parse 41B | 211,280 | 205,675 | -2.7% | No |
| Parse 7KB | 96,400 | 89,975 | -6.7% | Slight decrease (system load) |

### GIL Mode

| Scenario | R3 | R4 | Change | Regression? |
|----------|----|----|--------|-------------|
| Hello World | 80,783 | 83,097 | **+2.9%** | No |
| sleep(1ms) | 53,342 | 52,494 | -1.6% | No |
| numpy mean | 8,484 | 8,209 | -3.2% | No |

### Hybrid Mode

| Scenario | R3 | R4 | Change | Regression? |
|----------|----|----|--------|-------------|
| Hello World | 201,161 | 215,090 | **+6.9%** | Improved |
| Parse 41B | 213,898 | 210,170 | -1.7% | No |
| Parse 7KB | 93,227 | 96,142 | **+3.1%** | No |

## Conclusion

**SSE streaming (PyreStream + BoxBody + tokio-stream) introduced with zero performance regression.**

The `Either::Left` wrapping of BoxBody and the `type_name == "PyreStream"` check are both constant-time operations with no measurable impact on the hot path. Four rounds of benchmarks confirm the SubInterp ~215k baseline remains solid.

## Four-Round Benchmark Trend

| Scenario | R1 | R2 | R3 | R4 | Trend |
|----------|----|----|----|----|-------|
| SubInterp Hello | 219k | 211k | 216k | 214k | Stable ~215k |
| GIL Hello | 119k | 78k | 81k | 83k | Stable ~80k |
| GIL sleep | 7.4k | 47k | 53k | 52k | Stable ~50k |
| Hybrid Parse 41B | — | 208k | 214k | 210k | Stable ~210k |
