"""High-level Pyre application class with decorator syntax."""

from __future__ import annotations

import time
import sys
from typing import Callable

from skytrade.engine import SkyApp as _SkyApp, SkyResponse


class Pyre:
    """Pyre web framework — decorator-friendly wrapper around the Rust engine.

    Usage::

        from skytrade import Pyre, SkyResponse

        app = Pyre()

        @app.get("/")
        def index(req):
            return "Hello from Pyre!"

        @app.get("/hello/{name}")
        def greet(req):
            return {"message": f"Hello, {req.params['name']}!"}

        app.run()
    """

    def __init__(self) -> None:
        self._engine = _SkyApp()
        self._fallback_handler: Callable | None = None
        self._fallback_name: str | None = None

    # ------------------------------------------------------------------
    # Route registration (decorator + direct call)
    # ------------------------------------------------------------------

    def get(self, path: str, handler: Callable | None = None, *, gil: bool = False):
        return self._route("GET", path, handler, gil=gil)

    def post(self, path: str, handler: Callable | None = None, *, gil: bool = False):
        return self._route("POST", path, handler, gil=gil)

    def put(self, path: str, handler: Callable | None = None, *, gil: bool = False):
        return self._route("PUT", path, handler, gil=gil)

    def delete(self, path: str, handler: Callable | None = None, *, gil: bool = False):
        return self._route("DELETE", path, handler, gil=gil)

    def patch(self, path: str, handler: Callable | None = None, *, gil: bool = False):
        return self._route("PATCH", path, handler, gil=gil)

    def options(self, path: str, handler: Callable | None = None, *, gil: bool = False):
        return self._route("OPTIONS", path, handler, gil=gil)

    def head(self, path: str, handler: Callable | None = None, *, gil: bool = False):
        return self._route("HEAD", path, handler, gil=gil)

    def route(self, method: str, path: str, handler: Callable | None = None, *, gil: bool = False):
        return self._route(method.upper(), path, handler, gil=gil)

    def _route(self, method: str, path: str, handler: Callable | None, *, gil: bool = False):
        if handler is not None:
            self._engine.route(method, path, handler, gil)
            return handler

        def decorator(fn: Callable) -> Callable:
            self._engine.route(method, path, fn, gil)
            return fn

        return decorator

    # ------------------------------------------------------------------
    # Middleware
    # ------------------------------------------------------------------

    def before_request(self, handler: Callable | None = None):
        """Register a before-request hook. Use as decorator or direct call.

        The hook receives ``(request)`` and should return ``None`` to continue
        or a response to short-circuit.
        """
        if handler is not None:
            self._engine.before_request(handler)
            return handler

        def decorator(fn: Callable) -> Callable:
            self._engine.before_request(fn)
            return fn

        return decorator

    def after_request(self, handler: Callable | None = None):
        """Register an after-request hook. Use as decorator or direct call.

        The hook receives ``(request, response)`` and must return a
        ``SkyResponse``.
        """
        if handler is not None:
            self._engine.after_request(handler)
            return handler

        def decorator(fn: Callable) -> Callable:
            self._engine.after_request(fn)
            return fn

        return decorator

    # ------------------------------------------------------------------
    # Fallback (custom 404)
    # ------------------------------------------------------------------

    def fallback(self, handler: Callable | None = None):
        """Register a fallback handler for unmatched routes."""
        if handler is not None:
            self._engine.fallback(handler)
            return handler

        def decorator(fn: Callable) -> Callable:
            self._engine.fallback(fn)
            return fn

        return decorator

    # ------------------------------------------------------------------
    # WebSocket
    # ------------------------------------------------------------------

    def websocket(self, path: str, handler: Callable | None = None):
        """Register a WebSocket handler. Use as decorator or direct call.

        The handler receives a ``SkyWebSocket`` object with ``recv()``,
        ``send(msg)``, and ``close()`` methods::

            @app.websocket("/ws")
            def ws_handler(ws):
                while True:
                    msg = ws.recv()
                    if msg is None:
                        break
                    ws.send(f"echo: {msg}")
        """
        if handler is not None:
            self._engine.websocket(path, handler)
            return handler

        def decorator(fn: Callable) -> Callable:
            self._engine.websocket(path, fn)
            return fn

        return decorator

    # ------------------------------------------------------------------
    # Static files
    # ------------------------------------------------------------------

    def static(self, prefix: str, directory: str) -> None:
        """Serve static files from *directory* under URL *prefix*.

        Example::

            app.static("/static", "./public")
        """
        self._engine.static_dir(prefix, directory)

    # ------------------------------------------------------------------
    # Logging
    # ------------------------------------------------------------------

    def enable_logging(self) -> None:
        """Enable built-in request/response logging."""
        _timings: dict[int, float] = {}

        def _log_before(req):
            _timings[id(req)] = time.monotonic()
            return None

        def _log_after(req, resp):
            start = _timings.pop(id(req), None)
            elapsed = (time.monotonic() - start) * 1000 if start else 0
            status = getattr(resp, "status_code", 200)
            print(f"  {req.method} {req.path} → {status} ({elapsed:.1f}ms)")
            return resp

        self._engine.before_request(_log_before)
        self._engine.after_request(_log_after)

    # ------------------------------------------------------------------
    # Run
    # ------------------------------------------------------------------

    def run(
        self,
        host: str = "127.0.0.1",
        port: int = 8000,
        workers: int | None = None,
        mode: str | None = None,
    ) -> None:
        self._engine.run(host=host, port=port, workers=workers, mode=mode)
