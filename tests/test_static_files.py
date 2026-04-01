"""Tests for static file serving."""

import os
import tempfile
import pytest
from pyreframework import Pyre
from pyreframework.testing import TestClient


@pytest.fixture(scope="module")
def static_dir():
    """Create a temp directory with test static files."""
    d = tempfile.mkdtemp(prefix="pyre_static_")
    with open(os.path.join(d, "index.html"), "w") as f:
        f.write("<h1>Hello</h1>")
    with open(os.path.join(d, "style.css"), "w") as f:
        f.write("body { color: red; }")
    with open(os.path.join(d, "data.json"), "w") as f:
        f.write('{"key": "value"}')
    with open(os.path.join(d, "readme.txt"), "w") as f:
        f.write("plain text file")
    # Nested directory
    sub = os.path.join(d, "sub")
    os.makedirs(sub)
    with open(os.path.join(sub, "nested.js"), "w") as f:
        f.write("console.log('hi')")
    yield d


@pytest.fixture(scope="module")
def client(static_dir):
    app = Pyre()

    @app.get("/")
    def index(req):
        return {"api": True}

    app.static("/static/", static_dir)

    c = TestClient(app, port=19879)
    yield c
    c.close()


def test_static_html(client):
    resp = client.get("/static/index.html")
    assert resp.status_code == 200
    assert "<h1>Hello</h1>" in resp.text
    assert any("text/html" in v for v in resp.headers.values())


def test_static_css(client):
    resp = client.get("/static/style.css")
    assert resp.status_code == 200
    assert "color: red" in resp.text
    assert any("text/css" in v for v in resp.headers.values())


def test_static_json(client):
    resp = client.get("/static/data.json")
    assert resp.status_code == 200
    assert resp.json()["key"] == "value"


def test_static_txt(client):
    resp = client.get("/static/readme.txt")
    assert resp.status_code == 200
    assert "plain text file" in resp.text


def test_static_nested(client):
    resp = client.get("/static/sub/nested.js")
    assert resp.status_code == 200
    assert "console.log" in resp.text


def test_static_not_found(client):
    resp = client.get("/static/nonexistent.html")
    assert resp.status_code == 404


def test_static_path_traversal(client):
    resp = client.get("/static/../../../etc/passwd")
    assert resp.status_code == 403


def test_api_still_works(client):
    """API routes coexist with static files."""
    resp = client.get("/")
    assert resp.json()["api"] is True


def test_post_static_file_rejected(client):
    """POST to static file should return 404 (only GET/HEAD allowed)."""
    resp = client.post("/static/index.html")
    assert resp.status_code == 404
