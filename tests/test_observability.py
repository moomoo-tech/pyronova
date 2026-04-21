"""Tests for X-Request-ID propagation + Prometheus /metrics."""

from __future__ import annotations

import re

import pytest

from pyreframework import Pyre
from pyreframework.testing import TestClient


# ---------------------------------------------------------------------------
# X-Request-ID
# ---------------------------------------------------------------------------


def test_request_id_minted_when_absent():
    app = Pyre()
    app.enable_request_id()

    @app.get("/")
    def root(req):
        return "ok"

    with TestClient(app, port=None) as c:
        r = c.get("/")
        assert r.status_code == 200
        rid = r.headers.get("X-Request-ID") or r.headers.get("x-request-id")
        assert rid is not None
        # UUID hex: 32 chars, no dashes (uuid4().hex).
        assert re.fullmatch(r"[0-9a-f]{32}", rid)


def test_request_id_echoed_when_client_supplies():
    app = Pyre()
    app.enable_request_id()

    @app.get("/")
    def root(req):
        return "ok"

    with TestClient(app, port=None) as c:
        r = c.get("/", headers={"X-Request-ID": "trace-abc-123"})
        assert r.status_code == 200
        rid = r.headers.get("X-Request-ID") or r.headers.get("x-request-id")
        assert rid == "trace-abc-123"


def test_enable_request_id_idempotent():
    app = Pyre()
    app.enable_request_id()
    app.enable_request_id()  # no error, no double-hook

    @app.get("/")
    def root(req):
        return "ok"

    with TestClient(app, port=None) as c:
        r = c.get("/")
        rid_values = [v for k, v in r.headers.items() if k.lower() == "x-request-id"]
        # Exactly one header — not two copies from a double-install.
        assert len(rid_values) == 1


def test_custom_header_name():
    app = Pyre()
    app.enable_request_id(header="X-Trace-Id")

    @app.get("/")
    def root(req):
        return "ok"

    with TestClient(app, port=None) as c:
        r = c.get("/", headers={"X-Trace-Id": "custom-42"})
        assert r.headers.get("X-Trace-Id") == "custom-42" or \
               r.headers.get("x-trace-id") == "custom-42"


# ---------------------------------------------------------------------------
# Prometheus /metrics
# ---------------------------------------------------------------------------


def test_metrics_endpoint_content_type():
    app = Pyre()
    app.enable_metrics()

    @app.get("/")
    def root(req):
        return "ok"

    with TestClient(app, port=None) as c:
        r = c.get("/metrics")
        assert r.status_code == 200
        assert "text/plain" in r.headers.get("content-type", r.headers.get("Content-Type", ""))


def test_metrics_counts_requests():
    app = Pyre()
    app.enable_metrics()

    @app.get("/")
    def root(req):
        return "ok"

    @app.get("/boom")
    def boom(req):
        from pyreframework import PyreResponse
        return PyreResponse(body="broken", status_code=500)

    with TestClient(app, port=None) as c:
        # TestClient sends one GET / during startup to probe readiness;
        # snapshot the counter so we measure only what this test drives.
        base = _counter(c.get("/metrics").text, "pyre_http_requests_total")

        for _ in range(3):
            c.get("/")
        c.get("/boom")
        c.get("/boom")

        body = c.get("/metrics").text
        # Total: base + 3 + 2 + 1 (the previous /metrics scrape was
        # counted as a request by /metrics itself? no — skipped; but the
        # *handler* for /metrics runs, its after_request skips the
        # increment). Scrape skip confirmed in test_metrics_scrape_is_not_counted.
        assert _counter(body, "pyre_http_requests_total") == base + 5
        # Status-class and method breakdowns also advance by the known
        # deltas since `base`.
        assert _counter(body, 'pyre_http_requests_by_class_total{class="5xx"}') == 2


def test_metrics_scrape_is_not_counted():
    app = Pyre()
    app.enable_metrics()

    @app.get("/")
    def root(req):
        return "ok"

    with TestClient(app, port=None) as c:
        # Baseline after startup probe.
        base = _counter(c.get("/metrics").text, "pyre_http_requests_total")
        c.get("/")
        # Two more scrapes — they must NOT increment the counter.
        c.get("/metrics")
        body = c.get("/metrics").text
        assert _counter(body, "pyre_http_requests_total") == base + 1


def test_metrics_records_latency():
    app = Pyre()
    app.enable_metrics()

    @app.get("/slow")
    def slow(req):
        import time
        time.sleep(0.01)
        return "ok"

    with TestClient(app, port=None) as c:
        c.get("/slow")
        body = c.get("/metrics").text
        count = _counter(body, "pyre_http_request_duration_seconds_count")
        assert count == 1
        sum_seconds = _float(body, "pyre_http_request_duration_seconds_sum")
        # The 10ms sleep must land in the sum.
        assert sum_seconds >= 0.005


def test_metrics_format_has_help_and_type_lines():
    app = Pyre()
    app.enable_metrics()

    @app.get("/")
    def root(req):
        return "ok"

    with TestClient(app, port=None) as c:
        body = c.get("/metrics").text
        # Every counter has HELP + TYPE directives before the value.
        assert "# HELP pyre_http_requests_total" in body
        assert "# TYPE pyre_http_requests_total counter" in body
        assert "# HELP pyre_http_requests_by_class_total" in body
        assert "# TYPE pyre_http_requests_by_class_total counter" in body


def test_enable_metrics_idempotent():
    app = Pyre()
    app.enable_metrics()
    app.enable_metrics()  # no duplicate route

    @app.get("/")
    def root(req):
        return "ok"

    with TestClient(app, port=None) as c:
        base = _counter(c.get("/metrics").text, "pyre_http_requests_total")
        c.get("/")
        body = c.get("/metrics").text
        # Double-install would have added the after-hook twice and
        # counted every request as 2 — so the delta would be 2, not 1.
        assert _counter(body, "pyre_http_requests_total") == base + 1


def test_custom_metrics_path():
    app = Pyre()
    app.enable_metrics(path="/_/prom")

    @app.get("/")
    def root(req):
        return "ok"

    with TestClient(app, port=None) as c:
        c.get("/")
        r = c.get("/_/prom")
        assert r.status_code == 200
        assert "pyre_http_requests_total" in r.text
        # Default /metrics is NOT registered.
        assert c.get("/metrics").status_code == 404


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _counter(body: str, name: str) -> int:
    """Extract the integer value after ``name`` in Prometheus text output."""
    for line in body.splitlines():
        if line.startswith("#"):
            continue
        # Match "name value" — labels are part of name.
        if line.startswith(name + " "):
            return int(line.rsplit(" ", 1)[1])
    raise AssertionError(f"metric {name!r} not found in:\n{body}")


def _float(body: str, name: str) -> float:
    for line in body.splitlines():
        if line.startswith("#"):
            continue
        if line.startswith(name + " "):
            return float(line.rsplit(" ", 1)[1])
    raise AssertionError(f"metric {name!r} not found in:\n{body}")
