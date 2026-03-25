# Full Stack Benchmark: Pyre vs Robyn vs Pure Rust (2026-03-23)

Test environment: macOS ARM64 (Apple Silicon), Python 3.14.3, Rust 1.93.1, wrk 4.2.0
Parameters: `wrk -t4 -c256 -d10s`

## Tier Rankings

| Rank | Framework | GET / req/s | GET /hello req/s | Avg Latency | Memory (RSS) |
|------|------|------------|-----------------|----------|-----------|
| 1 | Actix-web (pure Rust) | 226,042 | 208,796 | 1.00ms | 11 MB |
| 2 | Axum (pure Rust, same stack as Pyre) | 223,845 | 220,891 | 0.86ms | 14 MB |
| 3 | **Pyre SubInterp** | **216,517** | **204,636** | **0.92ms** | **53 MB** |
| 4 | Pyre GIL | 100,673 | 97,727 | 2.56ms | 17 MB |
| 5 | Robyn --fast (22 processes) | 76,504 | 76,051 | 29.62ms | 420 MB |

## Python Overhead Analysis

| Comparison | GET / | GET /hello | Meaning |
|------|-------|-----------|------|
| Pyre SubInterp vs Axum (same stack) | 96.7% | 92.6% | Python only costs 3-7% |
| Pyre SubInterp vs Actix | 95.8% | 98.0% | Nearly on par with pure Rust |
| Pyre GIL vs Axum | 45.0% | 44.2% | Single GIL cuts performance in half |
| Robyn --fast vs Actix (same stack) | 33.8% | 36.4% | Loses 2/3 of performance |

## Key Conclusions

1. **Pyre SubInterp achieves 93-97% of pure Rust performance** -- Python handler overhead is nearly negligible
2. **The bottleneck is GIL contention, not Python execution speed** -- with the same Python handler, SubInterp is 2x faster than GIL mode
3. **Robyn's Actix-web backend is fast on its own** (226k req/s), but the Python layer loses 2/3 of that performance
4. **Pyre SubInterp is far more memory-efficient than Robyn** -- 53 MB vs 420 MB, 8x less
5. **Pure Rust Axum/Actix ceiling is around 220k req/s** -- Pyre SubInterp is already approaching the physical limit

## Technology Stack Correspondence

```
Pyre        = Tokio + Hyper + matchit   (~ Axum)
Robyn       = Tokio + Actix-web         (~ Actix-web)

Pyre SubInterp -> achieves 97% of Axum performance
Robyn --fast   -> achieves 34% of Actix performance
```
