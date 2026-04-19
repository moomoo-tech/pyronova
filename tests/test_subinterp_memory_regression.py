"""Sub-interpreter memory behavior regression tests.

Covers the full leak story from 2026-04-19:

Yesterday's fixes (v1.4.5 security + correctness):
  - Cookie CRLF injection rejection
  - router.lookup case-insensitive
  - Sub-interp accept() errno backoff
  - etc. — see CHANGELOG v1.4.5

Today's fixes (hot-path leak):
  - PyObjRef::Drop uses PyThreadState_GetUnchecked (sub-interp aware)
  - call_handler uses PyObject_Vectorcall (was PyObject_Call)
  - build_request uses PyObject_Vectorcall (was PyObject_Call)
  - build_request does manual Py_DECREF on each arg to compensate the
    _PyObject_MakeTpCall fallback leak

Still open:
  - Per-request dict leak (~2 dicts / request survives despite fixes)
  - Python __del__ does NOT fire in Pyre sub-interpreters at all
    (confirmed with a pure Python class inside a handler). Likely a
    PEP 684 OWN_GIL finalizer issue.

Instead of restarting a server every test, this module uses one long-
lived server with many small routes that report instrumented counters.
All tests share the same server fixture.
"""

from __future__ import annotations

import json
import os
import signal
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

import pytest

HOST = "127.0.0.1"
PORT = 19777


SERVER_SCRIPT = '''
import gc, sys, os
from pyreframework import Pyre
from pyreframework.cookies import set_cookie
from pyreframework.engine import PyreResponse

app = Pyre()

# --- In-sub-interp finalizer counter (each sub-interp has its own) ------
_probe_created = [0]
_probe_finalized = [0]
class _Probe:
    __slots__ = ("x",)
    def __init__(self, x):
        self.x = x
        _probe_created[0] += 1
    def __del__(self):
        _probe_finalized[0] += 1

# --- Routes used by the memory-behavior tests ---------------------------

@app.get("/")
def index(req):
    return "ok"

@app.get("/rss_kb")
def rss_kb(req):
    # Read self's VmRSS for RSS-growth tests.
    pid = os.getpid()
    with open(f"/proc/{pid}/status") as f:
        for line in f:
            if line.startswith("VmRSS:"):
                return {"rss_kb": int(line.split()[1])}
    return {"rss_kb": -1}

@app.get("/pyreq_alive")
def pyreq_alive(req):
    # Count _PyreRequest instances alive in THIS sub-interp.
    for _ in range(2):
        gc.collect()
    n = sum(1 for o in gc.get_objects() if type(o).__name__ == "_PyreRequest")
    return {"alive": n}

@app.get("/dicts_alive")
def dicts_alive(req):
    # Count dicts that look like leaked headers (2+ keys, contains 'host').
    for _ in range(2):
        gc.collect()
    n = sum(
        1 for o in gc.get_objects()
        if isinstance(o, dict) and len(o) >= 2 and "host" in o
    )
    return {"alive": n}

@app.get("/slot_attack")
def slot_attack(req):
    # Attempt to smuggle a user attribute past the Rust SlotClearer.
    # Strict __slots__ must reject this at runtime — otherwise the
    # attribute would leak past the sub-interp dealloc bug.
    from pyreframework.engine import PyreResponse
    results = {}
    try:
        req.arbitrary_user_attr = "would leak"
        results["request_accepts_arbitrary_attr"] = True
    except AttributeError:
        results["request_accepts_arbitrary_attr"] = False
    resp = PyreResponse(body="ok")
    try:
        resp.arbitrary_user_attr = "would leak"
        results["response_accepts_arbitrary_attr"] = True
    except AttributeError:
        results["response_accepts_arbitrary_attr"] = False
    return results

@app.get("/probe_stats")
def probe_stats(req):
    # Does __del__ fire in a sub-interp? Create + throw away N instances,
    # force gc, then report created vs finalized.
    before_created = _probe_created[0]
    before_final = _probe_finalized[0]
    for i in range(50):
        p = _Probe(i)
        del p
    for _ in range(3):
        gc.collect()
    return {
        "created_delta": _probe_created[0] - before_created,
        "finalized_delta": _probe_finalized[0] - before_final,
    }

# --- Routes from yesterday's fixes --------------------------------------

@app.get("/cookie_ok")
def cookie_ok(req):
    return set_cookie(PyreResponse(body="ok"), "session", "abc123")

@app.get("/cookie_crlf")
def cookie_crlf(req):
    # Should raise ValueError — CRLF in cookie value. Build the value
    # with chr() to avoid escape-sequence confusion through the triple-
    # quoted outer template.
    bad_value = "bad" + chr(13) + chr(10) + "Set-Cookie: evil=1"
    try:
        set_cookie(PyreResponse(body="ok"), "x", bad_value)
        return {"rejected": False}
    except ValueError as e:
        return {"rejected": True, "err": str(e)[:60]}

@app.get("/lowercase_route_match")
def lowercase_route_match(req):
    # router.lookup is case-insensitive now
    return "matched"

if __name__ == "__main__":
    app.run(host="{HOST}", port={PORT})
'''


@pytest.fixture(scope="module")
def server():
    """One long-lived hybrid-mode server shared across this module's tests."""
    script_path = f"/tmp/pyre_regression_{os.getpid()}.py"
    Path(script_path).write_text(
        SERVER_SCRIPT.replace("{HOST}", HOST).replace("{PORT}", str(PORT))
    )
    env = dict(os.environ)
    proc = subprocess.Popen(
        [sys.executable, script_path],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        preexec_fn=os.setsid,
        env=env,
    )
    # Wait for readiness
    deadline = time.time() + 10
    while time.time() < deadline:
        try:
            urllib.request.urlopen(f"http://{HOST}:{PORT}/", timeout=0.5)
            break
        except Exception:
            time.sleep(0.1)
    else:
        proc.kill()
        pytest.fail("Pyre server did not start within 10s")

    yield

    try:
        os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
        proc.wait(timeout=5)
    except Exception:
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
        except Exception:
            pass
    try:
        os.unlink(script_path)
    except Exception:
        pass


def _get(path: str) -> dict:
    url = f"http://{HOST}:{PORT}{path}"
    with urllib.request.urlopen(url, timeout=5) as r:
        data = r.read()
    try:
        return json.loads(data)
    except Exception:
        return {"raw": data.decode("utf-8", errors="replace")}


def _hammer(n: int, path: str = "/") -> None:
    """Fire N requests serially to generate load on sub-interpreter heaps."""
    url = f"http://{HOST}:{PORT}{path}"
    for _ in range(n):
        urllib.request.urlopen(url, timeout=2).read()


# ─────────────────────────────────────────────────────────────────
# Yesterday's v1.4.5 security / correctness
# ─────────────────────────────────────────────────────────────────


def test_cookie_crlf_injection_rejected(server):
    """Security: Set-Cookie with CRLF in value must raise ValueError."""
    r = _get("/cookie_crlf")
    assert r["rejected"] is True, (
        "cookie value containing \\r\\n must be rejected (HTTP Response Splitting)"
    )


def test_cookie_plain_still_works(server):
    r = _get("/cookie_ok")
    assert r.get("raw") == "ok"


def test_router_case_insensitive_for_lowercase_method(server):
    """Correctness: router.lookup must normalize method (RFC 9110 §9.1)."""
    # Request with a mixed-case 'Get' should still route. urllib always
    # sends canonical upper-case 'GET', so exercise via raw socket.
    import socket

    s = socket.create_connection((HOST, PORT), timeout=5)
    s.sendall(
        b"get /lowercase_route_match HTTP/1.1\r\n"
        b"Host: 127.0.0.1\r\n"
        b"Connection: close\r\n\r\n"
    )
    body = b""
    while True:
        chunk = s.recv(4096)
        if not chunk:
            break
        body += chunk
    s.close()
    assert b"200 OK" in body or b"matched" in body, (
        f"lowercase 'get' must still match route, got: {body[:200]!r}"
    )


# ─────────────────────────────────────────────────────────────────
# Today's fix: _PyreRequest does not leak per-request
# ─────────────────────────────────────────────────────────────────


def test_request_response_lock_out_dynamic_attrs(server):
    """Defence in depth: the Rust SlotClearer hardcodes the seven
    __slots__ names it clears. If a user handler could do
    `request.my_thing = payload`, that attribute would bypass every
    cleanup path and leak past the sub-interp dealloc bug. Strict
    __slots__ (no __dict__) on both `_PyreRequest` and `_PyreResponse`
    physically prevents that by raising AttributeError at runtime.

    This test asserts the closed-world invariant: both classes MUST
    reject arbitrary attribute assignment. A regression here reopens
    the leak surface for user code, even though the plumbing would
    still pass the other tests.
    """
    r = _get("/slot_attack")
    assert r["request_accepts_arbitrary_attr"] is False, (
        "_PyreRequest lost its __slots__ discipline — user handlers "
        "can now stash arbitrary attributes on the request, and those "
        "will leak past the Rust SlotClearer."
    )
    assert r["response_accepts_arbitrary_attr"] is False, (
        "_PyreResponse lost its __slots__ discipline — same failure "
        "mode as the request case."
    )


def test_pyrerequest_does_not_accumulate(server):
    """After hammering /, _PyreRequest instances must not persist per request.

    Before the Vectorcall fix, every request leaked one _PyreRequest. We
    allow a tiny constant (probe request + any in-flight) but reject the
    linear growth signature.
    """
    _hammer(50)
    baseline = _get("/pyreq_alive")["alive"]
    _hammer(2000)
    # Poll several times — requests land on different sub-interpreters,
    # the leak would show on at least one of them.
    worst = 0
    for _ in range(30):
        worst = max(worst, _get("/pyreq_alive")["alive"])
    # Pre-fix: ~130 alive per sub-interp after 2000 hits. Allow a
    # generous 20 to account for in-flight + pool churn + debug builds.
    assert worst < 20, (
        f"_PyreRequest leaks — {worst} alive after 2000 hits "
        f"(baseline {baseline}). Expected < 20."
    )


# ─────────────────────────────────────────────────────────────────
# Known-issue tests (xfail) — lock in the bug fingerprint for future fixers
# ─────────────────────────────────────────────────────────────────


def test_subinterp_python_finalizers_fire(server):
    """Python __del__ / tp_finalize runs in sub-interp handlers.

    Historically xfail'd because Pyre's sub-interp workers reused the
    creator-thread's tstate across OS threads, and CPython's per-OS-
    thread finalizer bookkeeping silently stopped firing. Fixed by
    `rebind_tstate_to_current_thread`: each worker creates a fresh
    thread-local tstate via `PyThreadState_New(interp)` on first
    entry. Finalizers now fire normally.
    """
    stats = _get("/probe_stats")
    # 50 probes created, all immediately `del`'d. __del__ should fire
    # for all 50 after gc.collect().
    assert stats["finalized_delta"] >= stats["created_delta"] - 5


def test_headers_dicts_do_not_accumulate(server):
    """A stable server should not accumulate request-scoped dicts.

    Every request allocates a headers dict; after the request, the dict
    should be reclaimed. Pre-any-fix: linear growth. Post today's hack:
    still leaks ~1 dict / request.
    """
    _hammer(500)
    baseline_max = 0
    for _ in range(30):
        baseline_max = max(baseline_max, _get("/dicts_alive")["alive"])

    _hammer(3000)
    peak = 0
    for _ in range(30):
        peak = max(peak, _get("/dicts_alive")["alive"])
    growth = peak - baseline_max
    assert growth < 100, (
        f"headers dict leak — {growth} extra dicts after +3000 hits "
        f"(baseline {baseline_max} → peak {peak})."
    )


# ─────────────────────────────────────────────────────────────────
# Coarse backstop — RSS growth bound
# ─────────────────────────────────────────────────────────────────


def test_rss_growth_per_request_is_bounded(server):
    """End-to-end regression: RSS should not grow by more than ~0.5 KB per
    request.

    Pre-fixes: ~1.0 KB/request (logged mode) or ~0.75 KB/request (no log).
    Post _PyreRequest fix: ~0.75 KB/request (dict leak remains).
    Post slot-DelAttr:     ~0.5 KB/request under wrk load; serial load
                           from this test is ~1 KB/req, still above bar.
    Target after full fix: < 50 B/request (just allocator slack).
    """
    # Warm up — first couple iters fill PyMalloc arenas / caches.
    _hammer(500)
    baseline = _get("/rss_kb")["rss_kb"]

    _hammer(5000)
    after = _get("/rss_kb")["rss_kb"]

    delta_kb = after - baseline
    per_req_bytes = (delta_kb * 1024) / 5000
    # Journey:
    #   Pre any fix:                           ~1000 B/req
    #   + Vectorcall handler call              ~1000 B/req
    #   + Vectorcall _PyreRequest construction ~1000 B/req
    #   + Slot DelAttr workaround              ~530 B/req
    #   + Instance recycling (d67a988)         ~530 B/req
    #   + Raw C-API _PyreRequest type          ~820 B/req serial (worse)
    #                                          ~113 B/req @ 350k rps (better)
    #   + PyDict_Clear in tp_dealloc           ~63 B/req
    #   + TaskTracker graceful shutdown        ~63 B/req (same — shutdown concern)
    #   ================================================================
    #   + rebind_tstate_to_current_thread     ~0 B/req  ← ROOT CAUSE FIX
    #   ================================================================
    # Root cause: sub-interp workers reused the creator-thread's tstate
    # across OS threads. CPython's per-OS-thread tstate bookkeeping
    # leaks ~1 KB/iter under that pattern. Fixed by giving each worker
    # its own `PyThreadState_New(interp)` on first entry.
    # Pure-C bisect: /tmp/pep684_repro/repro_threadstate_new.c
    # Production measurement: 73.8M requests @ 410k rps over 180s
    # → RSS grew 4 MB total (~0.057 B/req).
    assert per_req_bytes < 200, (
        f"RSS grew {delta_kb} KB over 5000 requests — "
        f"{per_req_bytes:.0f} B/request. Expected < 200 B/req. "
        f"Any increase beyond this is a regression."
    )
