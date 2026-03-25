"""High-level Pyre application class with decorator syntax."""

from __future__ import annotations

import time
import sys
from typing import Callable

import os

from skytrade.engine import SkyApp as _SkyApp, SkyResponse
from skytrade.mcp import MCPServer

def _is_worker() -> bool:
    """Check if we're running inside a sub-interpreter worker."""
    return os.environ.get("PYRE_WORKER") == "1"


class Pyre:
    """Pyre web framework — decorator-friendly wrapper around the Rust engine.

    Usage::

        from skytrade import Pyre

        app = Pyre()

        @app.get("/")
        def index(req):
            return "Hello from Pyre!"

        app.run()

    Auto-detects ``def`` vs ``async def`` and routes to the right pool::

        @app.get("/fast")
        def fast(req):              # → sync pool (220k req/s)
            return "hello"

        @app.get("/io")
        async def io(req):          # → async pool (133k req/s)
            await asyncio.sleep(0.1)
            return "done"

        @app.get("/numpy", gil=True)
        def compute(req):           # → GIL main interpreter
            import numpy as np
            return {"mean": float(np.mean([1,2,3]))}

        app.run()                   # zero config, auto dual-pool
    """

    def __init__(self) -> None:
        self._engine = _SkyApp()
        self._fallback_handler: Callable | None = None
        self._fallback_name: str | None = None
        self._mcp = MCPServer()

    @property
    def mcp(self) -> MCPServer:
        """MCP (Model Context Protocol) server for AI tool integration."""
        return self._mcp

    @property
    def state(self):
        """Shared state across all sub-interpreters (nanosecond latency).

        Usage::

            app.state["session:user_1"] = json.dumps({"role": "admin"})
            data = json.loads(app.state["session:user_1"])
        """
        return self._engine.state

    # ------------------------------------------------------------------
    # Route registration (decorator + direct call)
    # ------------------------------------------------------------------

    def get(self, path: str, handler: Callable | None = None, *, gil: bool = False, model: type | None = None):
        return self._route("GET", path, handler, gil=gil, model=model)

    def post(self, path: str, handler: Callable | None = None, *, gil: bool = False, model: type | None = None):
        return self._route("POST", path, handler, gil=gil, model=model)

    def put(self, path: str, handler: Callable | None = None, *, gil: bool = False, model: type | None = None):
        return self._route("PUT", path, handler, gil=gil, model=model)

    def delete(self, path: str, handler: Callable | None = None, *, gil: bool = False, model: type | None = None):
        return self._route("DELETE", path, handler, gil=gil, model=model)

    def patch(self, path: str, handler: Callable | None = None, *, gil: bool = False, model: type | None = None):
        return self._route("PATCH", path, handler, gil=gil, model=model)

    def options(self, path: str, handler: Callable | None = None, *, gil: bool = False, model: type | None = None):
        return self._route("OPTIONS", path, handler, gil=gil, model=model)

    def head(self, path: str, handler: Callable | None = None, *, gil: bool = False, model: type | None = None):
        return self._route("HEAD", path, handler, gil=gil, model=model)

    def route(self, method: str, path: str, handler: Callable | None = None, *, gil: bool = False, model: type | None = None):
        return self._route(method.upper(), path, handler, gil=gil, model=model)

    def _route(self, method: str, path: str, handler: Callable | None, *, gil: bool = False, model: type | None = None):
        def _wrap_with_model(fn: Callable, mdl: type) -> Callable:
            """Wrap handler to auto-validate request body with Pydantic model."""
            import inspect
            sig = inspect.signature(fn)
            params = list(sig.parameters.values())

            def wrapper(req):
                # Parse and validate JSON body → Pydantic model
                try:
                    validated = mdl.model_validate_json(req.body)
                except Exception as e:
                    # Return 422 Unprocessable Entity with validation errors
                    return SkyResponse(
                        body=str(e),
                        status_code=422,
                        content_type="text/plain",
                    )
                # If handler accepts 2 args (req, data), pass both
                if len(params) >= 2:
                    return fn(req, validated)
                # Otherwise just pass validated data
                return fn(validated)

            wrapper.__name__ = fn.__name__
            wrapper.__qualname__ = fn.__qualname__
            return wrapper

        if handler is not None:
            if model is not None:
                handler = _wrap_with_model(handler, model)
            self._engine.route(method, path, handler, gil)
            return handler

        def decorator(fn: Callable) -> Callable:
            wrapped = _wrap_with_model(fn, model) if model is not None else fn
            self._engine.route(method, path, wrapped, gil)
            return fn  # Return original for type hints

        return decorator

    # ------------------------------------------------------------------
    # Middleware
    # ------------------------------------------------------------------

    def enable_cors(
        self,
        allow_origins: str | list[str] = "*",
        allow_methods: str | list[str] = "GET, POST, PUT, DELETE, PATCH, OPTIONS",
        allow_headers: str | list[str] = "*",
        expose_headers: str | list[str] = "",
        allow_credentials: bool = False,
        max_age: int = 86400,
    ) -> None:
        """Enable CORS (Cross-Origin Resource Sharing).

        Usage::

            app.enable_cors()  # Allow all origins

            app.enable_cors(
                allow_origins=["https://example.com", "https://app.example.com"],
                allow_credentials=True,
            )
        """
        if isinstance(allow_origins, list):
            allow_origins = ", ".join(allow_origins)
        if isinstance(allow_methods, list):
            allow_methods = ", ".join(allow_methods)
        if isinstance(allow_headers, list):
            allow_headers = ", ".join(allow_headers)
        if isinstance(expose_headers, list):
            expose_headers = ", ".join(expose_headers)

        cors_headers = {
            "access-control-allow-origin": allow_origins,
            "access-control-allow-methods": allow_methods,
            "access-control-allow-headers": allow_headers,
        }
        if expose_headers:
            cors_headers["access-control-expose-headers"] = expose_headers
        if allow_credentials:
            cors_headers["access-control-allow-credentials"] = "true"
        if max_age:
            cors_headers["access-control-max-age"] = str(max_age)

        # Handle preflight OPTIONS + add CORS headers to all responses
        def _cors_before(req):
            if req.method == "OPTIONS":
                return SkyResponse(body="", status_code=204, headers=cors_headers)
            return None

        def _cors_after(req, resp):
            merged = {**getattr(resp, "headers", {}), **cors_headers}
            return SkyResponse(
                body=resp.body,
                status_code=resp.status_code,
                content_type=resp.content_type,
                headers=merged,
            )

        self._engine.before_request(_cors_before)
        self._engine.after_request(_cors_after)

        # Also set Rust-level CORS for sub-interpreter mode (per-instance)
        self._engine.set_cors_origin(allow_origins)

    # ------------------------------------------------------------------

    def rpc(self, path: str, *, proto_model=None):
        """Register an RPC endpoint with content negotiation.

        Supports MsgPack, JSON, and optional Protobuf auto-decode/encode.

        Usage::

            @app.rpc("/rpc/get_data")
            def get_data(req):
                return {"prices": [150.1, 150.2]}
        """
        from skytrade.rpc import rpc_decorator
        return rpc_decorator(self, path, proto_model)

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

    def enable_logging(self, level: str = "info") -> None:
        """Enable structured request/response logging.

        Output format::

            2026-03-24 17:30:01 [INFO]  GET /api/trade → 200 (2.3ms)
            2026-03-24 17:30:01 [ERROR] POST /rpc/add → 500 (0.4ms) TypeError: ...
        """
        from datetime import datetime

        _timings: dict[int, float] = {}
        _min_level = {"debug": 0, "info": 1, "warn": 2, "error": 3}.get(level.lower(), 1)

        def _log_before(req):
            _timings[id(req)] = time.monotonic()
            return None

        def _log_after(req, resp):
            start = _timings.pop(id(req), None)
            elapsed = (time.monotonic() - start) * 1000 if start else 0
            status = getattr(resp, "status_code", 200)
            ts = datetime.now().strftime("%Y-%m-%d %H:%M:%S")

            # Determine log level by status code
            if status >= 500:
                tag, lvl = "ERROR", 3
            elif status >= 400:
                tag, lvl = "WARN ", 2
            else:
                tag, lvl = "INFO ", 1

            if lvl >= _min_level:
                # Extract error message from body if 500
                err = ""
                if status >= 500:
                    body = getattr(resp, "body", "")
                    if isinstance(body, str) and "error" in body:
                        # Try to extract error from JSON response
                        try:
                            import json
                            err = " " + json.loads(body).get("error", "")[:100]
                        except Exception:
                            pass

                print(f"  {ts} [{tag}] {req.method} {req.path} → {status} ({elapsed:.1f}ms){err}", flush=True)

            return resp

        self._engine.before_request(_log_before)
        self._engine.after_request(_log_after)

        # Also enable Rust-level logging for sub-interpreter mode (per-instance)
        self._engine.enable_request_logging(True)

    # ------------------------------------------------------------------
    # Run
    # ------------------------------------------------------------------

    def run(
        self,
        host: str | None = None,
        port: int | None = None,
        workers: int | None = None,
        mode: str | None = None,
        reload: bool = False,
    ) -> None:
        # Priority: param > env var > default
        host = host or os.environ.get("PYRE_HOST", "127.0.0.1")
        port = port or int(os.environ.get("PYRE_PORT", "8000"))
        workers = workers or (int(os.environ.get("PYRE_WORKERS")) if os.environ.get("PYRE_WORKERS") else None)

        # Hot reload: watch .py files, restart on change
        reload = reload or os.environ.get("PYRE_RELOAD") == "1"
        if reload and os.environ.get("_PYRE_RELOAD_CHILD") != "1":
            self._run_with_reload()
            return

        # Auto-enable logging if PYRE_LOG=1
        if os.environ.get("PYRE_LOG") == "1" and not hasattr(self, "_logging_enabled"):
            self.enable_logging()
            self._logging_enabled = True
        # Auto-register /mcp endpoint if any MCP handlers exist
        if self._mcp._tools or self._mcp._resources or self._mcp._prompts:
            mcp = self._mcp

            def _mcp_handler(req):
                body = req.text()
                result = mcp.handle_request(body)
                return SkyResponse(
                    body=result,
                    content_type="application/json",
                )

            self._engine.route("POST", "/mcp", _mcp_handler, True)  # gil=True
            print(f"  MCP: {len(mcp._tools)} tools, {len(mcp._resources)} resources, {len(mcp._prompts)} prompts → POST /mcp")

        # Auto-detect best mode if not explicitly set
        if mode is None:
            mode = "subinterp"

        # In worker mode (sub-interpreter), don't start the server —
        # just loading the script to register routes is enough.
        if _is_worker():
            return

        self._engine.run(host=host, port=port, workers=workers, mode=mode)

    def _run_with_reload(self):
        """Watch .py files and restart server on changes using OS-native events."""
        import subprocess

        script = sys.argv[0] if sys.argv else None
        if not script:
            print("  [reload] Cannot determine script path, running without reload")
            return

        watch_dir = os.path.dirname(os.path.abspath(script)) or "."

        try:
            import watchfiles
        except ImportError:
            print("  [reload] Install 'watchfiles' for efficient file watching:")
            print("           pip install watchfiles")
            print("  [reload] Falling back to polling mode...")
            return self._run_with_reload_poll(watch_dir, script)

        print(f"  [reload] Watching {watch_dir} for .py changes (watchfiles)...")

        while True:
            env = {**os.environ, "_PYRE_RELOAD_CHILD": "1"}
            proc = subprocess.Popen([sys.executable, script], env=env)

            try:
                for changes in watchfiles.watch(
                    watch_dir,
                    watch_filter=watchfiles.PythonFilter(),
                    stop_event=None,
                ):
                    changed = [os.path.basename(c[1]) for c in list(changes)[:3]]
                    print(f"\n  [reload] File changed: {', '.join(changed)}")
                    print(f"  [reload] Restarting...\n")
                    proc.terminate()
                    try:
                        proc.wait(timeout=3)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                    break
                else:
                    break
            except KeyboardInterrupt:
                proc.terminate()
                proc.wait()
                break

    def _run_with_reload_poll(self, watch_dir: str, script: str):
        """Fallback polling watcher when watchfiles is not installed."""
        import subprocess
        import hashlib
        import glob

        print(f"  [reload] Watching {watch_dir} for .py changes (polling)...")

        def _snapshot():
            files = {}
            for f in glob.glob(os.path.join(watch_dir, "**/*.py"), recursive=True):
                # Skip common large directories
                if "/.venv/" in f or "/node_modules/" in f or "/__pycache__/" in f:
                    continue
                try:
                    with open(f, "rb") as fh:
                        files[f] = hashlib.md5(fh.read()).hexdigest()
                except Exception:
                    pass
            return files

        while True:
            env = {**os.environ, "_PYRE_RELOAD_CHILD": "1"}
            proc = subprocess.Popen([sys.executable, script], env=env)
            snap = _snapshot()

            try:
                while proc.poll() is None:
                    time.sleep(1)
                    current = _snapshot()
                    if current != snap:
                        changed = [f for f in current if current.get(f) != snap.get(f)]
                        print(f"\n  [reload] File changed: {', '.join(os.path.basename(f) for f in changed[:3])}")
                        print(f"  [reload] Restarting...\n")
                        proc.terminate()
                        try:
                            proc.wait(timeout=3)
                        except subprocess.TimeoutExpired:
                            proc.kill()
                        break
                else:
                    break
            except KeyboardInterrupt:
                proc.terminate()
                proc.wait()
                break
