"""Sanity tests for the access-log sampling knobs.

Doesn't try to capture actual log output — that needs cooperation from
tracing-subscriber + capture infra. These tests just exercise the
Rust-side wiring (enable_logging accepts sample / always_log_status,
set_request_log_sampling is callable, server starts cleanly with the
options set).
"""

import pytest

from pyronova import Pyronova
from pyronova.testing import TestClient


def test_enable_logging_with_sample_arg():
    app = Pyronova()
    app.enable_logging(level="info", sample=100, always_log_status=400)

    @app.get("/h")
    def handler(req):
        return "ok"

    with TestClient(app, port=None) as c:
        for _ in range(5):
            r = c.get("/h")
            assert r.status_code == 200


def test_set_request_log_sampling_directly():
    app = Pyronova()
    app.enable_logging()
    # Direct Rust setter — exercise the binding shape
    app._engine.set_request_log_sampling(50, 0)
    app._engine.set_request_log_sampling(1, 500)

    @app.get("/h")
    def handler(req):
        return "ok"

    with TestClient(app, port=None) as c:
        r = c.get("/h")
        assert r.status_code == 200


def test_sample_zero_clamped_to_one():
    """sample_n=0 would divide-by-zero — Rust clamps to 1."""
    app = Pyronova()
    app.enable_logging()
    app._engine.set_request_log_sampling(0, 0)  # would be UB without clamp

    @app.get("/h")
    def handler(req):
        return "ok"

    with TestClient(app, port=None) as c:
        for _ in range(3):
            assert c.get("/h").status_code == 200
