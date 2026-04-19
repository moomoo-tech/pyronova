# Pyre task runner. See docs/release-pipeline.md for the design.
# `just` instead of Makefile — no .PHONY noise, no tab/space traps,
# each recipe runs in its own shell.

set shell := ["bash", "-euo", "pipefail", "-c"]
set dotenv-load := false

# Default lists what's available.
default:
    @just --list

# ─── Build ───────────────────────────────────────────────────────

# Dev build — fast compile, debug symbols, leak_detect ON for free signal.
build:
    maturin develop --features leak_detect

# Release build — what we ship. LTO fat, strip, no leak_detect.
build-release:
    maturin develop --release

# Canary build — release codegen + leak_detect. Same perf as release
# (metrics hot path is ~1 ns), but the counters are live.
build-canary:
    maturin develop --release --features leak_detect

# ─── Compile gate ───────────────────────────────────────────────

# Compile-check both feature configs. Cheap sanity.
check:
    cargo check --lib
    cargo check --lib --features leak_detect

# ─── Test gate ──────────────────────────────────────────────────

test-rust:
    cargo test --lib

test-py: build-release
    .venv/bin/python -m pytest tests/ -q \
      --ignore=tests/test_ws_binary_client.py \
      --ignore=tests/multifile_app \
      -x

# Unit + e2e + memory regression. Covers both GIL + subinterp paths.
test: test-rust test-py

# ─── Benchmark ──────────────────────────────────────────────────

# Record a new baseline. Writes benchmarks/baseline.json. Do this
# deliberately — the file is committed, the commit message documents
# the machine and kernel.
bench-record: build-release
    #!/usr/bin/env bash
    set -euo pipefail
    nohup .venv/bin/python benchmarks/bench_plaintext.py > /tmp/pyre_bench_srv.log 2>&1 &
    pid=$!
    trap "kill $pid 2>/dev/null || true" EXIT
    # Wait for server
    for i in {1..40}; do
      curl -s http://127.0.0.1:8000/ >/dev/null 2>&1 && break
      sleep 0.25
    done
    # Warm + 3 measurement runs, take the best
    wrk -t4 -c100 -d3s http://127.0.0.1:8000/ >/dev/null 2>&1
    best=0
    for _ in 1 2 3; do
      rps=$(wrk -t4 -c100 -d10s http://127.0.0.1:8000/ 2>&1 | awk '/Requests\/sec:/ {print int($2)}')
      (( rps > best )) && best=$rps
    done
    machine=$(grep "model name" /proc/cpuinfo | head -1 | sed 's/.*: //')
    kernel=$(uname -r)
    python_v=$(.venv/bin/python -V | awk '{print $2}')
    now=$(date -u +%FT%TZ)
    cat > benchmarks/baseline.json <<EOF
    {
      "machine": "$machine",
      "kernel": "$kernel",
      "python": "$python_v",
      "recorded_at": "$now",
      "routes": {
        "GET /": { "req_per_sec": $best }
      }
    }
    EOF
    echo "recorded req_per_sec=$best to benchmarks/baseline.json"

# Run a short bench, compare against baseline, fail if regression > 5%.
bench-compare: build-release
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ ! -f benchmarks/baseline.json ]]; then
      echo "no baseline — run 'just bench-record' first"; exit 1
    fi
    nohup .venv/bin/python benchmarks/bench_plaintext.py > /tmp/pyre_bench_srv.log 2>&1 &
    pid=$!
    trap "kill $pid 2>/dev/null || true" EXIT
    for i in {1..40}; do
      curl -s http://127.0.0.1:8000/ >/dev/null 2>&1 && break
      sleep 0.25
    done
    wrk -t4 -c100 -d3s http://127.0.0.1:8000/ >/dev/null 2>&1
    best=0
    for _ in 1 2 3; do
      rps=$(wrk -t4 -c100 -d10s http://127.0.0.1:8000/ 2>&1 | awk '/Requests\/sec:/ {print int($2)}')
      (( rps > best )) && best=$rps
    done
    baseline=$(jq -r '.routes."GET /".req_per_sec' benchmarks/baseline.json)
    # Allow 5% below baseline.
    min=$(( baseline * 95 / 100 ))
    printf 'current=%d  baseline=%d  floor=%d\n' "$best" "$baseline" "$min"
    if (( best < min )); then
      echo "REGRESSION: $best < $min ($(( 100 * (baseline - best) / baseline ))% below baseline)"
      exit 1
    fi
    echo "OK"

# Bench the full-feature demo (examples/hello.py — enable_logging +
# CORS after_request hook + multiple routes). Captures the realistic
# cost of middleware + access logging on top of the engine. Compare
# against bench-record's plaintext-only number to see the hygiene tax.
# Not a release gate — throughput here is expected to be 5-10% lower.
bench-features: build-release
    #!/usr/bin/env bash
    set -euo pipefail
    nohup .venv/bin/python examples/hello.py > /tmp/pyre_bench_srv.log 2>&1 &
    pid=$!
    trap "kill $pid 2>/dev/null || true" EXIT
    for i in {1..40}; do
      curl -s http://127.0.0.1:8000/ >/dev/null 2>&1 && break
      sleep 0.25
    done
    # Warm + 3 measurement runs, take the best
    wrk -t4 -c100 -d3s http://127.0.0.1:8000/ >/dev/null 2>&1
    best=0
    for _ in 1 2 3; do
      rps=$(wrk -t4 -c100 -d10s http://127.0.0.1:8000/ 2>&1 | awk '/Requests\/sec:/ {print int($2)}')
      (( rps > best )) && best=$rps
    done
    printf 'full-feature (hello.py with logging + CORS hook): %d req/s\n' "$best"

# TechEmpower-style plaintext bench with HTTP pipelining depth 16.
# Uses wrk's shipped pipeline.lua (or our inline script) to issue
# batched requests — the canonical TFB "Plaintext" configuration.
bench-tfb-plaintext: build-release
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p /tmp/pyre_tfb
    cat > /tmp/pyre_tfb/pipeline.lua <<'LUA'
    init = function(args)
      local depth = tonumber(args[1]) or 16
      local r = {}
      for i = 1, depth do r[i] = wrk.format("GET", "/") end
      req = table.concat(r)
    end
    request = function() return req end
    LUA
    nohup .venv/bin/python benchmarks/bench_plaintext.py > /tmp/pyre_bench_srv.log 2>&1 &
    pid=$!
    trap "kill $pid 2>/dev/null || true" EXIT
    for i in {1..40}; do
      curl -s http://127.0.0.1:8000/ >/dev/null 2>&1 && break
      sleep 0.25
    done
    # Warm
    wrk -t8 -c256 -d5s --script /tmp/pyre_tfb/pipeline.lua http://127.0.0.1:8000/ -- 16 >/dev/null 2>&1
    echo "--- TFB Plaintext: -t8 -c256 -d15s --pipeline 16 ---"
    wrk -t8 -c256 -d15s --script /tmp/pyre_tfb/pipeline.lua --latency http://127.0.0.1:8000/ -- 16 | tail -12

# ─── Leak soak ──────────────────────────────────────────────────

# 5-minute load with leak_detect ON. Dumps the rc histogram, fails
# if any non-whitelisted type accumulates rc≥2 samples over time.
# Single-run check: pick up dramatic regressions (per-request leak at
# the 100% scale). Finer growth analysis still needs a human eye.
canary-soak: build-canary
    #!/usr/bin/env bash
    set -euo pipefail
    # GIL-routed /dump so leak_detect_dump is importable.
    cat > /tmp/pyre_canary_srv.py <<'PY'
    from pyreframework import Pyre
    app = Pyre()
    @app.get("/")
    def i(req):
        return "ok"
    @app.get("/_dump", gil=True)
    def d(req):
        from pyreframework.engine import leak_detect_dump
        leak_detect_dump()
        return "dumped"
    if __name__ == "__main__":
        app.run(host="127.0.0.1", port=8000)
    PY
    nohup .venv/bin/python /tmp/pyre_canary_srv.py 2>/tmp/pyre_canary.stderr >/dev/null &
    pid=$!
    trap "kill $pid 2>/dev/null || true; rm -f /tmp/pyre_canary_srv.py" EXIT
    for i in {1..40}; do
      curl -s http://127.0.0.1:8000/ >/dev/null 2>&1 && break
      sleep 0.25
    done
    echo "Soaking 5 minutes at 400k+ req/s..."
    wrk -t4 -c100 -d300s http://127.0.0.1:8000/ 2>&1 | tail -5
    curl -s http://127.0.0.1:8000/_dump > /dev/null
    sleep 1
    echo "--- leak histogram (type@rc) ---"
    grep "pyre_drop_rc" /tmp/pyre_canary.stderr | tee /tmp/pyre_canary.histogram
    # Known legit high-rc: interned singletons. Everything else growing
    # at rc>=2 fails the gate.
    #
    # Very simple filter: any non-whitelisted type at rc=2 with a count
    # >= 10% of total requests is a growing leak.
    total_requests=$(grep Requests/sec /tmp/pyre_canary.stderr | head -1 | awk '{print int($2 * 300)}' || echo 0)
    if [[ -z "$total_requests" || "$total_requests" == "0" ]]; then
      total_requests=10000000  # conservative — ~400k rps × 300s
    fi
    threshold=$(( total_requests / 10 ))
    problem=$(awk -v th=$threshold '
      /pyre_drop_rc\{type="str",rc="2"\}/ || /pyre_drop_rc\{type="str",rc="1"\}/ ||
      /pyre_drop_rc\{type="bytes",/ || /pyre_drop_rc\{type="tuple",/ ||
      /pyre_drop_rc\{type="type",/ || /pyre_drop_rc\{type="NoneType",/ ||
      /rc="1"/ || /rc="0"/ || /rc="9\+"/ { next }
      /pyre_drop_rc\{type="[^"]+",rc="[2-8]"\}/ {
        n = $NF + 0
        if (n > th) print
      }' /tmp/pyre_canary.histogram)
    if [[ -n "$problem" ]]; then
      echo ""
      echo "LEAK SUSPECT — non-whitelist type accumulates rc>=2 samples:"
      echo "$problem"
      exit 1
    fi
    echo "OK — no leak suspects above $threshold samples"

# ─── Release gate ───────────────────────────────────────────────

# Version fields agree across Cargo.toml, CHANGELOG.md heading, git tag.
version-sync:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo_v=$(awk -F'"' '/^version = / {print $2; exit}' Cargo.toml)
    changelog_v=$(awk '/^## v/ {print; exit}' CHANGELOG.md | sed -E 's/## v([0-9.]+).*/\1/')
    if [[ "$cargo_v" != "$changelog_v" ]]; then
      echo "MISMATCH: Cargo.toml=$cargo_v CHANGELOG=$changelog_v"
      exit 1
    fi
    echo "version synced at $cargo_v"

# Full pre-release validation. Run this before `git tag`.
release-gate: check test bench-compare canary-soak version-sync
    @echo ""
    @echo "✔ release-gate PASSED — safe to tag + push."

# Pre-push hook convenience — run the cheap half (no 5-min soak).
# Use this as the usual guard before `git push`.
pre-push: check test bench-compare
    @echo ""
    @echo "✔ pre-push PASSED — safe to push."

# ─── Housekeeping ───────────────────────────────────────────────

# Clean bench / leak artifacts, but keep the baseline file.
clean-bench:
    rm -f /tmp/pyre_bench_srv.log /tmp/pyre_canary.stderr /tmp/pyre_canary.histogram /tmp/pyre_canary_srv.py
    pkill -f "benchmarks/bench_plaintext.py" 2>/dev/null || true
    pkill -f "pyre_canary_srv" 2>/dev/null || true
