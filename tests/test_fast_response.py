"""Tests for ``app.add_fast_response`` — constant-body routes served
entirely from the Rust accept loop, no Python dispatch.
"""

from pyreframework import Pyre
from pyreframework.testing import TestClient


def test_fast_plain():
    app = Pyre()

    @app.get("/")
    def root(req):
        return "probe"

    app.add_fast_response("GET", "/health", b'{"ok":true}',
                         content_type="application/json")
    app.add_fast_response("GET", "/robots.txt",
                         b"User-agent: *\nDisallow: /\n")
    # str body also accepted (encoded as utf-8)
    app.add_fast_response("GET", "/ping", "pong", content_type="text/plain")

    with TestClient(app, port=None) as c:
        r = c.get("/health")
        assert r.status_code == 200
        assert r.body == b'{"ok":true}'
        assert r.headers.get("content-type", "").startswith("application/json")

        r = c.get("/robots.txt")
        assert r.status_code == 200
        assert b"User-agent" in r.body

        r = c.get("/ping")
        assert r.body == b"pong"


def test_fast_status_and_headers():
    app = Pyre()

    @app.get("/")
    def root(req):
        return "probe"

    app.add_fast_response(
        "GET", "/maintenance",
        b"we are down",
        content_type="text/plain",
        status_code=503,
        headers={"Retry-After": "30"},
    )

    with TestClient(app, port=None) as c:
        r = c.get("/maintenance")
        assert r.status_code == 503
        assert r.body == b"we are down"
        assert r.headers.get("retry-after") == "30" or r.headers.get("Retry-After") == "30"


def test_fast_does_not_interfere_with_dynamic():
    """A fast response on one path must not shadow a dynamic handler
    on a different path of the same method."""
    app = Pyre()

    @app.get("/")
    def root(req):
        return "probe"

    @app.get("/users/{id}")
    def user(req):
        return {"id": req.params["id"]}

    app.add_fast_response("GET", "/health", b"ok")

    with TestClient(app, port=None) as c:
        assert c.get("/health").body == b"ok"
        assert c.get("/users/42").json() == {"id": "42"}


def test_fast_exact_match_only():
    """Fast responses match (method, path) exactly — no path globbing."""
    app = Pyre()

    @app.get("/")
    def root(req):
        return "probe"

    # Only registers GET
    app.add_fast_response("GET", "/health", b"ok")

    @app.post("/health")
    def post_health(req):
        return "posted"

    with TestClient(app, port=None) as c:
        # GET uses fast path
        assert c.get("/health").body == b"ok"
        # POST still goes through the dynamic handler
        assert c.post("/health", body=b"").body == b"posted"


def test_bytes_body_fast_path_in_pyre_response():
    """PyreResponse with a bytes body takes the fast cast::<PyBytes>
    path instead of Vec<u8> extraction. Smoke test: the response still
    round-trips correctly."""
    from pyreframework import PyreResponse

    app = Pyre()

    @app.get("/")
    def root(req):
        return "probe"

    @app.get("/raw")
    def raw(req):
        return PyreResponse(b"\x00\x01\x02\xff", content_type="application/octet-stream")

    with TestClient(app, port=None) as c:
        r = c.get("/raw")
        assert r.body == b"\x00\x01\x02\xff"
        assert r.headers.get("content-type") == "application/octet-stream"
