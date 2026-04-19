"""TestClient — test Pyre routes without manually starting a server.

Usage::

    from pyreframework import Pyre
    from pyreframework.testing import TestClient

    app = Pyre()

    @app.get("/")
    def index(req):
        return {"hello": "world"}

    client = TestClient(app)
    resp = client.get("/")
    assert resp.status_code == 200
    assert resp.json()["hello"] == "world"
    client.close()
"""

from __future__ import annotations

import json
import threading
import time
import urllib.request
import urllib.error
from dataclasses import dataclass, field


@dataclass
class TestResponse:
    """Response from TestClient."""
    status_code: int
    body: bytes
    headers: dict[str, str] = field(default_factory=dict)

    @property
    def text(self) -> str:
        return self.body.decode("utf-8", errors="replace")

    def json(self) -> dict:
        return json.loads(self.body)


class TestClient:
    """Test client — starts Pyre in a background thread.

    Usage::

        client = TestClient(app)
        resp = client.get("/")
        assert resp.status_code == 200

        # Or as context manager:
        with TestClient(app) as client:
            resp = client.post("/data", body={"key": "value"})
    """

    def __init__(self, app, host: str = "127.0.0.1", port: int | None = 19876):
        # port=None auto-picks an unused port. Prefer this in new tests —
        # hard-coded ports collide across test files and produce
        # test-order-dependent flakes (two module fixtures bind the same
        # port, the second fails or worse, reuses the prior server).
        if port is None:
            import socket as _socket
            s = _socket.socket(_socket.AF_INET, _socket.SOCK_STREAM)
            s.bind((host, 0))
            port = s.getsockname()[1]
            s.close()
        self.host = host
        self.port = port
        self.base_url = f"http://{host}:{port}"

        # Start server in background thread (GIL mode for testing)
        self._thread = threading.Thread(
            target=lambda: app.run(host=host, port=port, mode="default"),
            daemon=True,
        )
        self._thread.start()

        # Wait for server ready
        for _ in range(50):
            time.sleep(0.1)
            try:
                urllib.request.urlopen(f"{self.base_url}/", timeout=1)
                return
            except Exception:
                pass

        raise RuntimeError("TestClient: server failed to start within 5s")

    def close(self):
        """Server runs as daemon thread — dies when main thread exits."""
        pass

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()

    def _request(
        self,
        method: str,
        path: str,
        body: bytes | str | dict | None = None,
        headers: dict[str, str] | None = None,
    ) -> TestResponse:
        url = f"{self.base_url}{path}"
        req_headers = headers or {}

        if isinstance(body, dict):
            body = json.dumps(body).encode("utf-8")
            req_headers.setdefault("Content-Type", "application/json")
        elif isinstance(body, str):
            body = body.encode("utf-8")

        req = urllib.request.Request(
            url, data=body, headers=req_headers, method=method
        )

        try:
            resp = urllib.request.urlopen(req, timeout=10)
            return TestResponse(
                status_code=resp.status,
                body=resp.read(),
                headers=dict(resp.headers),
            )
        except urllib.error.HTTPError as e:
            return TestResponse(
                status_code=e.code,
                body=e.read(),
                headers=dict(e.headers),
            )

    def get(self, path: str, **kwargs) -> TestResponse:
        return self._request("GET", path, **kwargs)

    def post(self, path: str, **kwargs) -> TestResponse:
        return self._request("POST", path, **kwargs)

    def put(self, path: str, **kwargs) -> TestResponse:
        return self._request("PUT", path, **kwargs)

    def delete(self, path: str, **kwargs) -> TestResponse:
        return self._request("DELETE", path, **kwargs)

    def patch(self, path: str, **kwargs) -> TestResponse:
        return self._request("PATCH", path, **kwargs)
