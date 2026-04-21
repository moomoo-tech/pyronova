"""Tests for PyreSettings + request-scoped ctx."""

from __future__ import annotations

import os

import pytest

from pyreframework import Pyre
from pyreframework.context import ctx
from pyreframework.testing import TestClient


# ---------------------------------------------------------------------------
# PyreSettings — skipped when pydantic-settings isn't installed
# ---------------------------------------------------------------------------


_has_pydantic_settings = True
try:
    import pydantic_settings  # noqa: F401
except ImportError:
    _has_pydantic_settings = False

skip_no_ps = pytest.mark.skipif(
    not _has_pydantic_settings,
    reason="pydantic-settings not installed",
)


@skip_no_ps
def test_settings_reads_env(monkeypatch):
    from pyreframework.config import PyreSettings

    class S(PyreSettings):
        database_url: str
        debug: bool = False

    monkeypatch.setenv("DATABASE_URL", "postgres://x")
    monkeypatch.setenv("DEBUG", "true")
    s = S()
    assert s.database_url == "postgres://x"
    assert s.debug is True


@skip_no_ps
def test_settings_case_insensitive(monkeypatch):
    from pyreframework.config import PyreSettings

    class S(PyreSettings):
        api_key: str

    monkeypatch.setenv("API_KEY", "secret")
    assert S().api_key == "secret"


@skip_no_ps
def test_settings_ignores_unknown_env(monkeypatch):
    """Unknown env vars don't crash the app — important for deployments
    where the env is shared across many services."""
    from pyreframework.config import PyreSettings

    class S(PyreSettings):
        port: int = 8000

    monkeypatch.setenv("SOMETHING_ELSE", "noise")
    assert S().port == 8000


# ---------------------------------------------------------------------------
# ctx
# ---------------------------------------------------------------------------


def test_ctx_set_get():
    ctx.clear()
    ctx.set("foo", 42)
    assert ctx.get("foo") == 42
    assert ctx.get("missing", "default") == "default"


def test_ctx_request_id_default_none():
    ctx.clear()
    assert ctx.request_id() is None


def test_ctx_populated_by_request_id_middleware():
    app = Pyre()
    app.enable_request_id()
    seen = {}

    @app.get("/")
    def root(req):
        seen["rid"] = ctx.request_id()
        seen["snap"] = ctx.snapshot()
        return "ok"

    with TestClient(app, port=None) as c:
        r = c.get("/", headers={"X-Request-ID": "trace-xyz"})
        assert r.status_code == 200
        assert seen["rid"] == "trace-xyz"
        # Snapshot is a detached copy — mutating it doesn't affect the
        # live ctx.
        seen["snap"]["foo"] = "bar"
        assert ctx.get("foo") is None


def test_ctx_isolated_between_requests():
    app = Pyre()
    app.enable_request_id()
    observed: list[str | None] = []

    @app.get("/set")
    def set_it(req):
        ctx.set("leaky", "from_first")
        return "ok"

    @app.get("/read")
    def read_it(req):
        observed.append(ctx.get("leaky"))
        return "ok"

    with TestClient(app, port=None) as c:
        c.get("/set")
        c.get("/read")
        # Second request started with a fresh ctx — it must not see the
        # value set by the first.
        assert observed == [None]
