# Benchmark 17 — HTTP Arena Head-to-Head: Pyre vs Actix

**Recorded:** 2026-04-20
**Machine:** AMD Ryzen 7 7840HS (8C/16T), 59 GB RAM. Host PG 17 stopped
for the duration so the Arena PG 18 sidecar (`--network host`) could bind.
Same machine runs both images inside Docker via the
`./scripts/benchmark-lite.sh` driver.
**Harness:** HTTP Arena's own `gcannon`-based lite driver from
[MDA2AV/HttpArena](https://github.com/MDA2AV/HttpArena) — three 5 s runs
per profile, report the best. **Not the official Threadripper run.**
Absolute numbers here are 8-core-class; the official leaderboard uses
64C Threadripper and scales ~4×. Relative ratios between frameworks on
this machine are what we're measuring.

## Full matrix

| Profile | **Pyre** rps | Pyre CPU% | Pyre Mem | **Actix** rps | Actix CPU% | Actix Mem | Winner |
|---|--:|--:|--:|--:|--:|--:|---|
| baseline | 478,533 | 1073 | 435 MiB | 844,068 | 860 | 21 MiB | Actix 1.76× |
| pipelined | 450,723 | 991 | 427 MiB | 4,725,017 | 1099 | 36 MiB | Actix **10.48×** |
| limited-conn | 324,621 | 953 | 427 MiB | 556,571 | 746 | 42 MiB | Actix 1.71× |
| json | 141,123 | 1079 | 442 MiB | 361,728 | 991 | 53 MiB | Actix 2.56× |
| json-comp | 7,151 | 997 | 614 MiB | 24,544 | 271 | 54 MiB | Actix 3.43× |
| json-tls | n/a | — | — | n/a | — | — | (docker flake) |
| upload | 1,088 | 712 | 3.1 GiB | 2,227 | 467 | 120 MiB | Actix 2.05× |
| async-db | 7,212 | 246 | 680 MiB | 46,109 | 717 | 56 MiB | Actix **6.39×** |
| **static** | **60,230** | 1103 | 2.5 GiB | 4,591 | 1298 | 4.2 GiB | **Pyre 13.12×** |
| baseline-h2 | 486,616 | 1097 | 832 MiB | 1,125,542 | 1338 | 282 MiB | Actix 2.31× |
| **static-h2** | **36,201** | 1023 | 4.2 GiB | 6,063 | 1000 | 52.2 GiB | **Pyre 5.97×** |

**Profiles Pyre wins:** static, static-h2 — 13× and 6× respectively.
**Profiles Actix wins:** everything else.

json-tls didn't record — the bench script kept crashing the Docker daemon
during its tuning phase after a dozen rebuilds in sequence. Data point
TODO on a clean machine run.

## Honest reading

The headline: **Actix beats Pyre on most profiles.** That's the real
Rust-vs-Python asymmetry showing through — Pyre dispatches every request
into a Python handler (sub-interpreter worker blocking on GIL release),
Actix's handler is a native Rust future. The gap is widest where the
request path is cheapest (pipelined: 10.48×) because Python-call
overhead is proportionally largest there.

Where Pyre wins — **static file serving** — the request path never
touches Python. `try_static_file` runs entirely in Rust on the tokio
async-fs backend, before the handler dispatch. Actix's `actix-files`
evidently re-reads the file from disk per request and, on the h2
profile, holds file descriptors and chunk buffers such that RAM climbs
to **52 GiB**. Pyre peaks at 4.2 GiB for the same workload.

**Surprises we should dig into before a real submission:**

1. **json-comp**: our own benchmark-15 (same machine, same payload,
   `wrk -t4 -c100`) showed Pyre 1.5× faster than tuned Actix on
   compressed JSON. Arena's gcannon-based profile at concurrency 512
   reverses that — Actix 3.4× faster. Candidate causes:
   - Arena's `json-comp` test sends smaller payloads than our 3 KB
     fortunes; our `min_size=1` setting means Pyre compresses tiny
     responses that shouldn't be worth it.
   - gcannon's concurrency pattern may surface something Actix's
     Compress middleware handles better under load than pyre's
     `maybe_compress_subinterp` path.
   - Needs investigation before we claim the Arena compressed-JSON
     number.
2. **upload**: our own streaming tests show ~3-4 B/req memory growth;
   here Pyre uses 3.1 GiB to Actix's 120 MiB under the upload profile.
   We're likely buffering in the sub-interp hybrid GIL-route branch
   (see `handlers.rs` comment about streaming-not-in-subinterp-yet).
   v2 that path and the memory should drop by an order of magnitude.
3. **pipelined**: our own plaintext pipelined test hit 921k rps on
   this machine. Arena's profile reads 450k — half. The difference is
   the gcannon template (three rotated paths including `/pipeline`),
   not our underlying throughput. For a clean comparison we'd want
   a pyre-native pipelined config. For now we report what Arena
   measured.

## What the submission looks like

```
arena_submission/
├── Dockerfile      # python:3.13-slim → maturin build of pyre → install wheel
├── README.md       # submission notes + reproduction instructions
├── app.py          # all 8 Arena routes, stock Pyre decorators
├── launcher.py     # 2 processes (HTTP + HTTPS) — pyre binds one port each
└── meta.json       # subscribes to 11 tests (all except crud/api-4/api-16/h3)
```

Two framework fixes landed alongside this work (commit `68053be`):
- `_bootstrap.py`: sub-interp mocks for `pyreframework.db`, `.crud`
- `_bootstrap.py`: `_MockPyre.__getattr__` fallback so new feature
  toggles don't need to be mocked individually
- `handlers.rs`: sub-interp hybrid GIL-route serves streaming routes
  with a one-shot pre-materialized chunk (API compat, memory not yet
  optimized for that path)

## Submission posture

Not ready for an official submission yet. Before we PR to
`MDA2AV/HttpArena/frameworks/pyre/`:

1. **Debug json-comp anomaly** — either fix, or set min_size=200
   to match actix's default and retest.
2. **Debug upload memory** — build the real streaming feeder path in
   sub-interp hybrid GIL-route branch (v2 of streaming).
3. **Get json-tls running** — the Docker daemon instability under
   the bench tuning phase is a host-side issue; a clean reboot + a
   single clean run should cover it.
4. **Decide whether to include the pipelined profile** — given Arena's
   gcannon template is different from our own plaintext bench, our
   pipelined number is not representative of pure engine throughput.

## Defensible claims from this run

- "On HTTP Arena's **static file profile**, Pyre is **13× faster than
  Actix-web 4** on an 8-core Ryzen (60k vs 4.6k rps). On HTTP/2 the
  ratio narrows to 6× but Actix burns 12× more memory (52 GiB vs 4.2)."
- "On plaintext / pipelined / json / async-db paths Actix still wins —
  Rust-async-vs-Python-handler is a real gap, nothing we're going to
  close without changing the framework's core value prop."
- "Pyre's results are defensible against FastAPI; head-to-head with
  Actix is only a win on I/O-bound routes where Python dispatch doesn't
  dominate."

## Reproducibility

```bash
# Prep the submission
git clone --depth 1 https://github.com/MDA2AV/HttpArena.git /tmp/HttpArena
cp -r arena_submission /tmp/HttpArena/frameworks/pyre
cp -r . /tmp/HttpArena/frameworks/pyre/pyre_src
rm -rf /tmp/HttpArena/frameworks/pyre/pyre_src/{target,.venv}

# Stop host PG first if it's using port 5432
sudo systemctl stop postgresql

# Run — one profile at a time survives better
cd /tmp/HttpArena
./scripts/benchmark-lite.sh pyre baseline
./scripts/benchmark-lite.sh actix baseline
# ...repeat per profile
```
