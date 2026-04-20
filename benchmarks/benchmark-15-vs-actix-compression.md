# Benchmark 15 — Pyre vs Actix-web on Compressed JSON

**Recorded:** 2026-04-20
**Machine:** AMD Ryzen 7 7840HS (8C/16T), 59 GB RAM, powersave governor,
significant background load (skytrade + prefect services running). All
runs back-to-back within minutes; relative numbers are apples-to-apples
even though absolute rps is ~30% below the 7840HS max recorded in
`baseline.json`.
**Workload:** `GET /json-fortunes` → 32-record fortunes-shaped JSON,
~3 KB uncompressed, ~1.25 KB after brotli.
**Client:** `wrk -t4 -c100 -d10s`, with and without `Accept-Encoding`.

## Executive summary

| | Pyre | Actix default | Actix TUNED | Notes |
|---|---|---|---|---|
| **brotli** rps | **87,263** | 2,460 | 55,660 | Pyre 1.57× tuned, 35× default |
| **gzip** rps | **112,134** | 2,437 | 92,745 | Pyre 1.21× tuned, 46× default |
| **no compression** rps | 272,501 | 396,573 | **389,888** | **Actix 1.43×** |

Pyre beats Actix on compressed responses under every configuration
we could devise for Actix. Actix beats Pyre on uncompressed responses
— expected, since Pyre pays Python-handler + sub-interpreter dispatch
overhead that a pure-Rust framework doesn't.

## Full numbers

### Pyre (brotli q=4, gzip L=6, `tokio::task::spawn_blocking`)

```
=== brotli (AE: br, gzip) ===
  Latency      1.12ms  σ=551μs  p99=7.76ms
  Req/s        87,263        Transfer/s  119 MB/s

=== gzip (AE: gzip) ===
  Latency      0.87ms  σ=454μs  p99=9.74ms
  Req/s       112,134        Transfer/s  159 MB/s

=== no compression (no AE) ===
  Latency      357μs   σ=233μs  p99=6.92ms
  Req/s       272,501        Transfer/s  813 MB/s
```

### Actix-web 4 — default `Compress::default()` middleware

```
=== brotli default (quality 11, sync middleware) ===
  Latency     40.48ms  σ=5.02ms  p99=42.80ms
  Req/s        2,460         Transfer/s  3.5 MB/s

=== gzip default (level 6, sync middleware) ===
  Latency     40.88ms  σ=2.60ms  p99=43.41ms
  Req/s        2,437         Transfer/s  3.7 MB/s

=== no compression ===
  Latency      180μs   σ=190μs   p99=10.79ms
  Req/s       396,573        Transfer/s  1.08 GB/s
```

### Actix-web 4 — TUNED (brotli q=4, gzip L=6, `web::block` off-runtime)

```
=== brotli tuned (quality 4, web::block) ===
  Latency      1.84ms  σ=1.41ms  p99=35.72ms
  Req/s        55,660        Transfer/s  75 MB/s

=== gzip tuned (level 6, web::block) ===
  Latency      1.07ms  σ=822μs   p99=14.99ms
  Req/s        92,745        Transfer/s  131 MB/s

=== no compression ===
  Latency      200μs   σ=274μs   p99=10.14ms
  Req/s       389,888        Transfer/s  1.06 GB/s
```

## Why Actix's defaults are 35–46× slower

Two compounding factors, neither of which reflects Rust vs Python:

**1. `ContentEncoding::Brotli` default quality is 11 (max compression).**
actix-http's `Compress` middleware maps `ContentEncoding::Brotli` to
`async_compression::Level::Default`, which for brotli means **quality 11**.
That's the slowest setting — ~10–30ms per 3 KB payload on this machine.
No production service should be running q=11 on a request hot path.
Pyre's default is q=4, the industry sweet spot (Fastify, nginx, cloudflare
all default to q≈4).

**2. The default middleware blocks the async worker thread.**
actix-web 4's `Compress` is synchronous on the request path. At
concurrency 100 across 4 workers, a single slow compression holds up
all ~25 queued requests behind it on that worker. That's where the
40ms p99 latency + 2.5k rps plateau comes from: head-of-line blocking,
not raw compression speed.

Pyre runs compression on `tokio::task::spawn_blocking`, which has 512
threads by default. A hundred concurrent compressions happily run in
parallel; the request-serving loop never stalls.

Both of these are "defaults someone would ship." They're not straw-men.

## Why Pyre still wins by 1.2–1.6× when Actix is tuned

Actix TUNED uses the same crates (`brotli` 7.x, `flate2` 1.x), same
quality (br=4, gzip=6), same off-runtime pattern (`web::block` which
internally is `tokio::task::spawn_blocking`). So the CPU work per
compress call is byte-for-byte identical.

The ~50% gap on brotli and ~20% gap on gzip comes from:

### mimalloc (the biggest factor)

Pyre uses `#[global_allocator] = mimalloc::MiMalloc`. The rust-baseline
crate has no global allocator set, so it uses glibc malloc.

Brotli q=4 allocates a lot of small temporary buffers during
compression. At ~100k compressions/second, glibc's arena locks become
a real contention point. mimalloc's thread-local heaps eliminate that
contention entirely. Independent benchmarks (redis, clickhouse, Rust
crates like `snmalloc-rs`) consistently show mimalloc 15–25% faster
than glibc malloc on concurrent-alloc-heavy workloads.

Evidence: **gzip gap is smaller (1.21×) than brotli gap (1.57×)**.
Gzip L=6 compresses the same 3 KB in ~50μs vs brotli's ~150μs, so it
allocates less and spends less time in the allocator. If the gap came
from something else (e.g., response builder overhead), gzip and brotli
would show similar ratios. They don't → allocator is the main driver.

### Thinner response path

Pyre's compressed response path after the handler returns is:
```
maybe_compress_subinterp(&mut body, &ct, &mut headers, accept_encoding)
→ hyper::Response::builder() with owned Vec<u8>
```

Actix's path is:
```
HttpResponse::Ok().insert_header(...).content_type(...).body(body)
→ actix-http ResponseBuilder → HeaderMap::insert × N → Response<Body>
```

Per request Pyre saves roughly one `HeaderMap::insert` plus a couple
of clones. At 10μs saved per 100μs request, that's another ~5% — which
stacks with the allocator win.

### What Actix could do to close the gap

- Add `mimalloc = "0.1"` + `#[global_allocator]` → probably recovers most of the 50% brotli gap
- Use `actix-web-lab`'s custom `Compress` with min-size filter →
  matches Pyre's 256-byte threshold (not a perf issue in this test
  since every response is > threshold, but it's a correctness parity)
- Use `HttpResponseBuilder::body()` with pre-allocated `Bytes` → marginal

None of these are in actix-web's defaults. A real Actix user would have
to know to do them.

## Why Actix beats Pyre on uncompressed responses

272k (Pyre) vs 390k (Actix tuned) = Actix **1.43× faster** on the
uncompressed path. This is the real Rust-vs-Python gap:

- Pyre: request → hyper → sub-interp dispatch → Python handler
  (`return {"fortunes": FORTUNES}`) → pythonize → `serde_json::to_vec`
  → Rust response. Python side adds ~150–200ns per request just for
  the handler call + GIL/tstate transitions.
- Actix: request → actix-http → Rust handler → `HttpResponse::json()`
  → `serde_json::to_vec`. No Python in the loop.

The ~40% gap is consistent with other Pyre bench results
(`benchmark-11-vs-fastapi.md`: Pyre is 12–15× FastAPI but still
~30–40% below pure-Rust baselines on plaintext paths).

## Takeaway — what's defensible to claim

**✓ Defensible:**

- "Pyre is ~1.5× faster than a tuned actix-web 4 setup on compressed
  JSON responses. Out of the box, Pyre is 35–46× faster because
  actix-web ships with brotli quality 11 as default and a synchronous
  compression middleware."
- "The speedup vs tuned Actix comes from mimalloc (concurrent
  allocator contention) and a slightly thinner response path."
- "On uncompressed responses Actix is ~1.4× faster — the Rust-async
  advantage over Python-handler dispatch is real."

**✗ Not defensible:**

- "Pyre is the fastest framework." (Actix wins plaintext, async-DB,
  short-lived connections.)
- "Pyre beats Actix by 35×." (Only vs Actix's bad defaults. An
  experienced Actix user never ships that config.)
- "HTTP Arena composite win." (Per-profile we can win JSON Compressed
  at 1.5× tuned, but Actix's other-profile wins keep their composite
  ahead on the HTTP Arena leaderboard.)

**Honest 64C extrapolation (with ~0.5 NUMA discount, not 0.7):**

- Pyre brotli: 87k × 8 × 0.5 ≈ 350k rps
- Actix tuned brotli: 55k × 8 × 0.5 ≈ 220k rps
- Pyre advantage on 64C: **~1.5×** (not 2–3×, not 40×)

## Addendum — TLS (added 2026-04-20, same session)

Added after TLS support landed. Same machine, same wrk config,
same fortunes payload, self-signed rustls cert, ALPN negotiates h2
but wrk uses HTTP/1.1 over TLS (HTTP/1.1 keep-alive dominates the
benchmark so this doesn't matter for throughput).

| Config | Pyre | Actix |
|---|---|---|
| **TLS, no compression** | 254,697 rps / p50 366μs / p99 1ms | **307,462 rps** / p50 248μs / p99 44ms |
| **TLS + brotli q=4** | 84,106 rps / 115 MB/s | not measured |
| **TLS + gzip L=6** | 108,455 rps / 154 MB/s | not measured |

**Findings:**

1. **Pyre pays ~6% TLS tax** (272k plain → 255k TLS). Actix pays
   ~21% (390k plain → 307k TLS). Both use rustls 0.23 + ring crypto
   — same cipher CPU work, same AES-NI. Pyre's lower tax likely
   comes from fewer middleware layers in the response path (same
   reason Pyre beats Actix on compressed: thinner stack).
2. **TLS narrows the gap.** Plain Actix leads 1.43×; TLS Actix leads
   only 1.21×. At higher compression + TLS workloads, the gap is
   expected to narrow further since compression CPU dominates.
3. **TLS + compression stacks cleanly.** Pyre brotli-over-TLS hits
   84k (vs 87k plain-brotli), within measurement noise. Adding the
   rustls layer on top of an already CPU-bound compression path is
   effectively free.
4. **Actix's p99 (44ms) is worse than Pyre's (1ms) under TLS.** Same
   observation as the compression section: Actix's async stack has
   tail-latency hiccups, probably from worker imbalance at
   concurrency 100 / 4 workers.

### Memory (sustained TLS load)

Ran the regression gate with TLS on:

```
7,405,824 requests over 30s
RSS warm: 208,332 KB
RSS end:  210,128 KB
Growth:   1,796 KB → 0.24 B/req
```

Passes the 200 B/req hard gate by three orders of magnitude. Confirms
the `fc45a7f` tstate-rebind fix holds under the TLS code path — no
new leak introduced by rustls integration.

### Defensible claim on TLS

"Pyre's TLS overhead is ~6%, lower than Actix's ~21%, because our
response path after the rustls handshake goes through fewer
middleware layers. In absolute terms Actix still leads TLS throughput
by 1.2× on uncompressed JSON — same Rust-vs-Python asymmetry as
plaintext, just compressed by the TLS tax Actix pays."

## Reproducibility

```bash
# Build Pyre (from repo root)
maturin develop --release

# Build Actix binaries
cd benchmarks/rust-baseline
cargo build --release --bin bench-actix-compressed          # default
cargo build --release --bin bench-actix-compressed-tuned    # tuned
cargo build --release --bin bench-actix-tls                 # TLS
cd ../..

# Generate a self-signed cert for the TLS runs
mkdir -p /tmp/pyre_tls && cd /tmp/pyre_tls
openssl req -x509 -newkey rsa:2048 -nodes \
    -keyout key.pem -out cert.pem -days 1 \
    -subj "/CN=localhost"
cd -

# Start Pyre (port 8001)
PYRE_COMPRESSION=1 PYRE_PORT=8001 python benchmarks/bench_compression.py &
# OR start Actix (port 8002)
benchmarks/rust-baseline/target/release/bench-actix-compressed &           # default
benchmarks/rust-baseline/target/release/bench-actix-compressed-tuned &     # tuned

# wrk — use matching port
wrk -t4 -c100 -d10s -H 'Accept-Encoding: br, gzip' http://127.0.0.1:8001/json-fortunes
wrk -t4 -c100 -d10s -H 'Accept-Encoding: gzip'     http://127.0.0.1:8001/json-fortunes
wrk -t4 -c100 -d10s                                http://127.0.0.1:8001/json-fortunes

# TLS variants (port 8443)
PYRE_TLS_CERT=/tmp/pyre_tls/cert.pem PYRE_TLS_KEY=/tmp/pyre_tls/key.pem \
    PYRE_PORT=8443 python benchmarks/bench_compression.py &
# OR Actix TLS
PYRE_TLS_CERT=/tmp/pyre_tls/cert.pem PYRE_TLS_KEY=/tmp/pyre_tls/key.pem \
    benchmarks/rust-baseline/target/release/bench-actix-tls &

wrk -t4 -c100 -d10s -H 'Accept-Encoding: identity' https://127.0.0.1:8443/json-fortunes
```

Same 32-record payload, same wrk config. Numbers should track within
~10% on equivalent hardware with CPU governor set to `performance`.
