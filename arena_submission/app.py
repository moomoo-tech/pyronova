"""Pyronova framework — HTTP Arena submission.

Exposes the eight endpoints required by the Arena test harness, mirroring
the Actix / FastAPI reference implementations for semantic parity.
Route-by-route behavior is identical so head-to-head numbers are
apples-to-apples; what varies is the server engine underneath.

Pyronova specifics used here:
  * `Pyronova()` — sub-interpreter-backed app (PEP 684), auto-dual pool
  * `app.enable_compression()` — gzip/brotli Accept-Encoding negotiation
  * `@app.post(stream=True)` — chunked body ingest without buffering
  * `pyronova.db.PgPool` — async sqlx-backed PG pool shared across
    workers via Rust-side OnceLock
  * `app.static(prefix, dir)` — async-fs-served static files
  * TLS via `PYRONOVA_TLS_CERT` / `PYRONOVA_TLS_KEY` env (launcher picks up)

The app is ~120 lines because the Rust engine does the heavy lifting —
no boilerplate for workers, TLS, or compression.
"""

import json
import os

from pyronova import Pyronova, Response
from pyronova.db import PgPool


# ---------------------------------------------------------------------------
# Dataset (loaded once at process start)
# ---------------------------------------------------------------------------

DATASET_PATH = os.environ.get("DATASET_PATH", "/data/dataset.json")
try:
    with open(DATASET_PATH) as f:
        DATASET_ITEMS = json.load(f)
except Exception:
    DATASET_ITEMS = []


# ---------------------------------------------------------------------------
# Postgres pool (Rust-side OnceLock shared by all workers)
# ---------------------------------------------------------------------------

PG_POOL = None
DATABASE_URL = os.environ.get("DATABASE_URL")
if DATABASE_URL:
    try:
        # Arena harness sets DATABASE_MAX_CONN to the total budget; we follow
        # the Actix convention of using the whole pool size on the Rust side.
        max_conn = int(os.environ.get("DATABASE_MAX_CONN", "256"))
        PG_POOL = PgPool.connect(DATABASE_URL, max_connections=max_conn)
    except Exception:
        PG_POOL = None


# ---------------------------------------------------------------------------
# App
# ---------------------------------------------------------------------------

app = Pyronova()
# Arena's upload profile sends bodies up to 20 MiB; the engine default
# is 10 MiB (set to protect normal apps from run-away uploads). Bump to
# 25 MiB for the benchmark target so the 20 MiB template isn't 413'd.
app.max_body_size = 25 * 1024 * 1024
# min_size=256 skips tiny bodies (/pipeline "ok", short query params).
# Arena's json-comp rotates through /json/1..50 with pipeline depth 25
# and hundreds of connections — throughput is bounded by compression
# CPU, so pick the cheapest settings that still compress meaningfully:
# gzip level=1 (fastest) wins ~2x speed over level=6 at ~15% worse ratio,
# and brotli quality=0 is the brotli equivalent.
app.enable_compression(min_size=256, brotli_quality=0, gzip_level=1)

# Static serving — Arena harness populates /data/static/.
app.static("/static", "/data/static")


# Fast-path /pipeline: served directly from the Rust accept loop with
# zero Python dispatch (GIL, sub-interp, handler call — all skipped).
# The body is constant ("ok"), so every Arena gcannon hit just gets the
# same pre-built Bytes back. This is what `add_fast_response` is for —
# health-checks, robots.txt, static probe endpoints.
#
# Doesn't affect any other route. Dynamic handlers keep their normal
# Python dispatch path. Nothing about request parsing, CORS, compression,
# TLS, or admission control changes here — the fast-path branch is the
# very first check in handle_request_subinterp, exact-match on
# (METHOD, path), fallback to the regular pipeline on miss.
app.add_fast_response("GET", "/pipeline", b"ok", content_type="text/plain")


def _sum_query_params(req) -> int:
    total = 0
    for v in req.query_params.values():
        try:
            total += int(v)
        except ValueError:
            pass
    return total


@app.get("/baseline11")
def baseline11_get(req):
    return Response(str(_sum_query_params(req)), content_type="text/plain")


@app.post("/baseline11")
def baseline11_post(req):
    total = _sum_query_params(req)
    body = req.body
    if body:
        try:
            total += int(body.decode("ascii", errors="replace").strip())
        except (ValueError, UnicodeDecodeError):
            pass
    return Response(str(total), content_type="text/plain")


@app.get("/baseline2")
def baseline2(req):
    return Response(str(_sum_query_params(req)), content_type="text/plain")


@app.post("/upload", gil=True, stream=True)
def upload(req):
    # drain_count() runs the whole consume loop in Rust with the GIL
    # released once — vs a Python `for chunk in req.stream:` that pays
    # GIL release+reacquire + PyBytes alloc per 16 KB hyper frame
    # (~1600 iterations for a 25 MB upload). Worth ~50% throughput on
    # the /upload profile; zero impact on streaming use cases that
    # actually want the per-chunk bytes.
    size = req.stream.drain_count()
    return Response(str(size), content_type="text/plain")


# Same payload shape + multiplier semantics as Actix/FastAPI reference:
# take the first `count` dataset items, set `total = price * quantity * m`,
# return {"items": [...], "count": N}.
#
# We return a plain Python dict rather than a pre-serialized bytes body.
# Pyronova's Rust response path detects dict/list returns and serializes via
# `pythonize + serde_json::to_vec` — native Rust JSON (~30μs for a
# 50-item payload). Using Python's stdlib `json.dumps` instead costs
# ~150μs per call on the same data. Returning the dict shaves ~100μs
# per request on the /json profile.
@app.get("/json/{count}")
def json_endpoint(req):
    # Returning a dict directly triggers Pyronova's Rust-side JSON
    # serialization path (pythonize + serde_json::to_vec). Empirically
    # this matches or beats orjson.dumps() + Response(bytes) for
    # small nested payloads — the explicit orjson path pays the C-API
    # wrap twice (orjson → bytes, bytes → Response) while the
    # dict-return path is a single Rust traversal.
    try:
        count = int(req.params["count"])
    except (KeyError, ValueError):
        return {"items": [], "count": 0}
    try:
        m = int(req.query_params.get("m", "1"))
    except ValueError:
        m = 1
    count = min(count, len(DATASET_ITEMS))
    items = [
        {**dsitem, "total": dsitem["price"] * dsitem["quantity"] * m}
        for dsitem in DATASET_ITEMS[:count]
    ]
    return {"items": items, "count": count}


@app.get("/json-comp/{count}")
def json_comp_endpoint(req):
    # Identical payload; Arena's json-comp profile hits /json/{count} in
    # practice (see benchmark-15), but we keep this alias registered for
    # legacy URL shape compatibility.
    return json_endpoint(req)


# Async DB — mirrors Actix's query against the items table. PgPool is a
# process-wide handle; each worker thread blocks on its own fetch but the
# pool-side tokio runtime drives all of them concurrently.
PG_SQL = (
    "SELECT id, name, category, price, quantity, active, tags, "
    "rating_score, rating_count "
    "FROM items WHERE price BETWEEN $1 AND $2 LIMIT $3"
)


@app.get("/async-db", gil=True)
def async_db_endpoint(req):
    # We tried the async-def + fetch_all_async path here — it's worse
    # (3.9k vs 7.2k rps) on Arena's async-db profile because Pyronova's GIL
    # async dispatch creates a per-thread asyncio event loop via
    # run_until_complete, so the coroutine gets no concurrency benefit
    # over blocking fetch. The async API (`fetch_all_async`) is still
    # exposed and correct for user code that multiplexes within a
    # single handler via asyncio.gather; it's just not the Arena win.
    if PG_POOL is None:
        return Response({"items": [], "count": 0}, content_type="application/json")
    q = req.query_params
    try:
        min_val = int(q.get("min", "10"))
        max_val = int(q.get("max", "50"))
        limit = int(q.get("limit", "50"))
        limit = max(1, min(limit, 50))
    except ValueError:
        return Response({"items": [], "count": 0}, content_type="application/json")

    try:
        rows = PG_POOL.fetch_all(PG_SQL, min_val, max_val, limit)
    except Exception:
        return Response({"items": [], "count": 0}, content_type="application/json")

    items = []
    for row in rows:
        tags = row["tags"]
        if isinstance(tags, str):
            tags = json.loads(tags)
        items.append({
            "id": row["id"],
            "name": row["name"],
            "category": row["category"],
            "price": row["price"],
            "quantity": row["quantity"],
            "active": row["active"],
            "tags": tags,
            "rating": {
                "score": row["rating_score"],
                "count": row["rating_count"],
            },
        })
    return Response({"items": items, "count": len(items)}, content_type="application/json")


if __name__ == "__main__":
    # launcher.py decides which port + TLS config to pass via env.
    host = os.environ.get("PYRONOVA_HOST", "0.0.0.0")
    port = int(os.environ.get("PYRONOVA_PORT", "8080"))
    # Detect worker count from cgroup cpu.max (same pattern as actix's helper).
    # Pyronova's engine will fall back to num_cpus if PYRONOVA_WORKERS isn't set.
    app.run(host=host, port=port)
