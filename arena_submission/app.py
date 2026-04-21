"""Pyre framework — HTTP Arena submission.

Exposes the eight endpoints required by the Arena test harness, mirroring
the Actix / FastAPI reference implementations for semantic parity.
Route-by-route behavior is identical so head-to-head numbers are
apples-to-apples; what varies is the server engine underneath.

Pyre specifics used here:
  * `Pyre()` — sub-interpreter-backed app (PEP 684), auto-dual pool
  * `app.enable_compression()` — gzip/brotli Accept-Encoding negotiation
  * `@app.post(stream=True)` — chunked body ingest without buffering
  * `pyreframework.db.PgPool` — async sqlx-backed PG pool shared across
    workers via Rust-side OnceLock
  * `app.static(prefix, dir)` — async-fs-served static files
  * TLS via `PYRE_TLS_CERT` / `PYRE_TLS_KEY` env (launcher picks up)

The app is ~120 lines because the Rust engine does the heavy lifting —
no boilerplate for workers, TLS, or compression.
"""

import json
import os

from pyreframework import Pyre, PyreResponse
from pyreframework.db import PgPool


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

app = Pyre()
app.enable_compression(min_size=1, brotli_quality=4, gzip_level=5)

# Static serving — Arena harness populates /data/static/.
app.static("/static", "/data/static")


@app.get("/pipeline")
def pipeline(req):
    return PyreResponse(b"ok", content_type="text/plain")


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
    return PyreResponse(str(_sum_query_params(req)), content_type="text/plain")


@app.post("/baseline11")
def baseline11_post(req):
    total = _sum_query_params(req)
    body = req.body
    if body:
        try:
            total += int(body.decode("ascii", errors="replace").strip())
        except (ValueError, UnicodeDecodeError):
            pass
    return PyreResponse(str(total), content_type="text/plain")


@app.get("/baseline2")
def baseline2(req):
    return PyreResponse(str(_sum_query_params(req)), content_type="text/plain")


@app.post("/upload", gil=True, stream=True)
def upload(req):
    size = 0
    for chunk in req.stream:
        size += len(chunk)
    return PyreResponse(str(size), content_type="text/plain")


# Same payload shape + multiplier semantics as Actix/FastAPI reference:
# take the first `count` dataset items, set `total = price * quantity * m`,
# return {"items": [...], "count": N}.
def _build_json_body(count: int, m: float) -> bytes:
    count = min(count, len(DATASET_ITEMS))
    items = []
    for dsitem in DATASET_ITEMS[:count]:
        item = dict(dsitem)
        item["total"] = dsitem["price"] * dsitem["quantity"] * m
        items.append(item)
    return json.dumps({"items": items, "count": len(items)}).encode()


@app.get("/json/{count}")
def json_endpoint(req):
    try:
        count = int(req.params["count"])
    except (KeyError, ValueError):
        return PyreResponse({"items": [], "count": 0}, status_code=200)
    try:
        m = float(req.query_params.get("m", "1"))
    except ValueError:
        m = 1.0
    body = _build_json_body(count, m)
    return PyreResponse(body, content_type="application/json")


@app.get("/json-comp/{count}")
def json_comp_endpoint(req):
    # Identical payload; Arena's json-comp profile sends Accept-Encoding so
    # the Compress middleware does the encoding.
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
    if PG_POOL is None:
        return PyreResponse({"items": [], "count": 0}, content_type="application/json")
    q = req.query_params
    try:
        min_val = int(q.get("min", "10"))
        max_val = int(q.get("max", "50"))
        limit = int(q.get("limit", "50"))
        limit = max(1, min(limit, 50))
    except ValueError:
        return PyreResponse({"items": [], "count": 0}, content_type="application/json")

    try:
        rows = PG_POOL.fetch_all(PG_SQL, min_val, max_val, limit)
    except Exception:
        return PyreResponse({"items": [], "count": 0}, content_type="application/json")

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
    return PyreResponse({"items": items, "count": len(items)}, content_type="application/json")


if __name__ == "__main__":
    # launcher.py decides which port + TLS config to pass via env.
    host = os.environ.get("PYRE_HOST", "0.0.0.0")
    port = int(os.environ.get("PYRE_PORT", "8080"))
    # Detect worker count from cgroup cpu.max (same pattern as actix's helper).
    # Pyre's engine will fall back to num_cpus if PYRE_WORKERS isn't set.
    app.run(host=host, port=port)
