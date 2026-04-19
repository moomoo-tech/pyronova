"""End-to-end HTTP cookie roundtrip — distinct from test_cookies_unit.py
which tests the pure-Python helpers without a server.
"""

import os

from tests.conftest import feature_server_factory

SERVER = '''
import os, json
from pyreframework import Pyre, PyreResponse
from pyreframework.cookies import get_cookie, set_cookie

app = Pyre()

@app.get("/__ping")
def ping(req):
    return "pong"

@app.get("/set")
def set_route(req):
    return set_cookie(PyreResponse(body="ok"), "sid", "abc123", httponly=True)

@app.get("/get")
def get_route(req):
    return {"sid": get_cookie(req, "sid", "none")}

if __name__ == "__main__":
    app.run(
        host="127.0.0.1",
        port=int(os.environ["PYRE_PORT"]),
        mode=os.environ["PYRE_MODE"],
    )
'''

feature_server = feature_server_factory(SERVER)


def test_set_cookie_header_includes_flags(feature_server):
    status, _, headers = feature_server.get("/set")
    assert status == 200
    # Response headers are case-insensitive; check both capitalisations.
    cookie = headers.get("Set-Cookie") or headers.get("set-cookie") or ""
    assert "sid=abc123" in cookie
    assert "HttpOnly" in cookie


def test_get_cookie_reads_request_header(feature_server):
    import json as _json
    status, body, _ = feature_server.get("/get", headers={"Cookie": "sid=xyz789"})
    assert status == 200
    assert _json.loads(body)["sid"] == "xyz789"
