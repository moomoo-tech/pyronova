"""End-to-end CORS header emission on normal and error responses.

Regression guard for the v1.4.5 CORS fixes: allow_credentials +
expose_headers on actual (non-preflight) responses, and CORS applied
on the sub-interp Err path (browser used to see opaque CORS failures
instead of real 5xx errors).
"""

import json
import os

from tests.conftest import feature_server_factory

SERVER = '''
import os
from pyreframework import Pyre, PyreResponse

app = Pyre()
app.enable_cors()

@app.get("/__ping")
def ping(req):
    return "pong"

@app.get("/")
def index(req):
    return {"ok": True}

@app.get("/boom")
def boom(req):
    # Trigger a handler error to exercise the Err path
    raise RuntimeError("intentional test error")

if __name__ == "__main__":
    app.run(
        host="127.0.0.1",
        port=int(os.environ["PYRE_PORT"]),
        mode=os.environ["PYRE_MODE"],
    )
'''

feature_server = feature_server_factory(SERVER)


def _cors_origin(headers: dict) -> str:
    return (
        headers.get("Access-Control-Allow-Origin")
        or headers.get("access-control-allow-origin")
        or ""
    )


def test_cors_header_on_ok_response(feature_server):
    status, _, headers = feature_server.get("/")
    assert status == 200
    assert _cors_origin(headers) == "*"


def test_cors_header_on_err_response(feature_server):
    """The Err path of the sub-interp handler previously omitted CORS
    headers, so the browser would surface a CORS failure instead of the
    real 5xx. Fixed in v1.4.5."""
    status, _, headers = feature_server.get("/boom")
    # We don't care about the exact status (could be 500); only that
    # CORS was applied so the browser can read the error body.
    assert status >= 400
    assert _cors_origin(headers) == "*", (
        f"CORS missing on error response (mode={feature_server.mode})"
    )
