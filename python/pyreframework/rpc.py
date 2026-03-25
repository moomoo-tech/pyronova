"""Pyre RPC — MsgPack/JSON/Protobuf content-negotiated RPC over HTTP.

Server: @app.rpc("/method") decorator with auto-decode/encode.
Client: PyreRPCClient with __getattr__ magic for local-like calls.
"""

from __future__ import annotations

import json
import inspect
from typing import Callable

try:
    import msgpack
    HAS_MSGPACK = True
except ImportError:
    HAS_MSGPACK = False


class PyreRPCClient:
    """Magic RPC client — call remote methods like local functions.

    Usage::

        client = PyreRPCClient("http://127.0.0.1:8000")
        result = client.get_market_snapshot(tickers=["AAPL", "TSLA"])
    """

    def __init__(self, base_url: str, use_msgpack: bool = True):
        import httpx
        self.base_url = base_url.rstrip("/")
        self.use_msgpack = use_msgpack and HAS_MSGPACK
        self._client = httpx.Client(
            http2=False,
            limits=httpx.Limits(max_connections=100, max_keepalive_connections=20),
        )

    def __getattr__(self, method_name: str):
        def remote_call(**kwargs):
            if self.use_msgpack:
                payload = msgpack.packb(kwargs, use_bin_type=True)
                content_type = "application/msgpack"
            else:
                payload = json.dumps(kwargs).encode("utf-8")
                content_type = "application/json"

            resp = self._client.post(
                f"{self.base_url}/rpc/{method_name}",
                content=payload,
                headers={
                    "Content-Type": content_type,
                    "Accept": content_type,
                },
            )

            if self.use_msgpack and "msgpack" in resp.headers.get("content-type", ""):
                data = msgpack.unpackb(resp.content, raw=False)
            else:
                data = resp.json()

            if not data.get("ok", True):
                raise RuntimeError(f"RPC Error: {data.get('error')}")

            return data.get("result", data)

        return remote_call

    def close(self):
        self._client.close()

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()


def rpc_decorator(app, path: str, proto_model=None):
    """Create an RPC endpoint with content negotiation.

    Supports MsgPack, JSON, and optional Protobuf.
    Auto-wraps response in {"ok": true, "result": ...} envelope.
    """

    def decorator(fn: Callable) -> Callable:
        is_async = inspect.iscoroutinefunction(fn)

        def _decode_request(req):
            if not req.body:
                return {}
            ct = req.headers.get("content-type", "application/json").lower()
            if HAS_MSGPACK and "msgpack" in ct:
                return msgpack.unpackb(req.body, raw=False)
            elif "protobuf" in ct and proto_model:
                return proto_model().parse(req.body)
            else:
                return json.loads(req.text())

        def _encode_response(result, req):
            accept = req.headers.get("accept", req.headers.get("content-type", "")).lower()
            envelope = {"ok": True, "result": result}

            if HAS_MSGPACK and "msgpack" in accept:
                from pyreframework.engine import PyreResponse
                body = msgpack.packb(envelope, use_bin_type=True)
                return PyreResponse(body=body, content_type="application/msgpack")
            else:
                return envelope  # Framework auto-serializes dict as JSON

        # Check if handler takes 2 args (req, data) or 1 (req)
        sig = inspect.signature(fn)
        takes_data = len(sig.parameters) >= 2

        def sync_wrapper(req):
            try:
                data = _decode_request(req)
                result = fn(req, data) if takes_data else fn(data)
                return _encode_response(result, req)
            except Exception as e:
                return {"ok": False, "error": f"{type(e).__name__}: {e}"}

        async def async_wrapper(req):
            try:
                data = _decode_request(req)
                result = await (fn(req, data) if takes_data else fn(data))
                return _encode_response(result, req)
            except Exception as e:
                return {"ok": False, "error": f"{type(e).__name__}: {e}"}

        handler = async_wrapper if is_async else sync_wrapper
        handler.__name__ = fn.__name__
        handler.__qualname__ = fn.__qualname__

        # Register as POST route with gil=True (RPC typically needs full Python)
        app._engine.route("POST", path, handler, True)
        return fn

    return decorator
