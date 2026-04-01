"""Tests for TestClient and HTTP verbs."""

import pytest
from pyreframework import Pyre, PyreResponse
from pyreframework.testing import TestClient


@pytest.fixture(scope="module")
def client():
    app = Pyre()

    @app.get("/")
    def index(req):
        return {"hello": "world"}

    @app.get("/text")
    def text(req):
        return "plain text"

    @app.get("/user/{name}")
    def user(req):
        return {"name": req.params["name"]}

    @app.post("/echo")
    def echo(req):
        return req.json()

    @app.put("/put")
    def put_handler(req):
        return {"method": "PUT", "data": req.json()}

    @app.delete("/del/{id}")
    def delete_handler(req):
        return {"deleted": req.params["id"]}

    @app.get("/status")
    def custom_status(req):
        return PyreResponse(body="created", status_code=201)

    @app.get("/headers")
    def custom_headers(req):
        return PyreResponse(body="ok", headers={"x-custom": "test"})

    c = TestClient(app, port=19877)
    yield c
    c.close()


def test_get_json(client):
    resp = client.get("/")
    assert resp.status_code == 200
    assert resp.json()["hello"] == "world"


def test_get_text(client):
    resp = client.get("/text")
    assert resp.status_code == 200
    assert resp.text == "plain text"


def test_path_params(client):
    resp = client.get("/user/alice")
    assert resp.json()["name"] == "alice"


def test_post_json(client):
    resp = client.post("/echo", body={"key": "value"})
    assert resp.status_code == 200
    assert resp.json()["key"] == "value"


def test_put(client):
    resp = client.put("/put", body={"x": 1})
    assert resp.json()["method"] == "PUT"
    assert resp.json()["data"]["x"] == 1


def test_delete(client):
    resp = client.delete("/del/42")
    assert resp.json()["deleted"] == "42"


def test_custom_status(client):
    resp = client.get("/status")
    assert resp.status_code == 201


def test_custom_headers(client):
    resp = client.get("/headers")
    # Header keys are case-insensitive
    assert any(k.lower() == "x-custom" for k in resp.headers)


def test_404(client):
    resp = client.get("/nonexistent")
    assert resp.status_code == 404


# ---------------------------------------------------------------------------
# CORS and binary data regression tests
# ---------------------------------------------------------------------------

@pytest.fixture(scope="module")
def cors_client():
    app = Pyre()
    app.enable_cors()

    @app.get("/")
    def index(req):
        return {"ok": True}

    @app.get("/binary")
    def binary(req):
        return PyreResponse(
            body=b"\xff\xd8\xff\xe0\x00\x10JFIF",
            content_type="image/jpeg",
        )

    c = TestClient(app, port=19878)
    yield c
    c.close()


def test_cors_on_normal_response(cors_client):
    resp = cors_client.get("/")
    assert resp.status_code == 200
    headers_lower = {k.lower(): v for k, v in resp.headers.items()}
    assert "access-control-allow-origin" in headers_lower


def test_cors_no_duplicates(cors_client):
    """CORS headers should not be duplicated (W3C spec violation)."""
    resp = cors_client.get("/")
    # Count occurrences of access-control-allow-origin
    cors_count = sum(
        1 for k in resp.headers if k.lower() == "access-control-allow-origin"
    )
    assert cors_count <= 1, f"Duplicate CORS headers: {cors_count}"


def test_binary_response_preserved(cors_client):
    """Binary data must not be corrupted by UTF-8 lossy conversion."""
    resp = cors_client.get("/binary")
    assert resp.status_code == 200
    assert resp.body[:2] == b"\xff\xd8", f"JPEG magic bytes corrupted: {resp.body[:4]}"
