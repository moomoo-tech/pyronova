"""Bootstrap script injected into each sub-interpreter worker.

Provides mock pyreframework modules so user scripts can be executed in isolated
sub-interpreters without importing the real Rust extension (which doesn't
support PEP 684 multi-interpreter loading).

Also installs PyreRustHandler to hijack Python's logging module — all log
records are routed through the _pyre_emit_log C-FFI function into Rust's
tracing system for zero-GIL-blocking I/O.

WARNING: This replaces `sys.modules["pydantic"]` with a no-op stub.
Pydantic validation is only available on routes with `gil=True`.
Sub-interpreter routes get a stub that lets `from pydantic import BaseModel`
succeed but does NOT perform real validation.
"""

# -- Python logging bridge to Rust tracing -----------------------------------

import logging as _logging

class _PyreRustHandler(_logging.Handler):
    """Routes Python logging records through Rust tracing via C-FFI.

    _pyre_emit_log is injected into globals by Rust (interp.rs) before
    this bootstrap script runs. It accepts (level, name, message, pathname,
    lineno, worker_id) and dispatches to tracing macros with near-zero cost.
    """

    def __init__(self, worker_id=0):
        super().__init__()
        self._worker_id = worker_id

    def emit(self, record):
        try:
            msg = record.getMessage()
            # Preserve exception tracebacks (logger.exception / exc_info=True)
            if record.exc_info and not record.exc_text:
                record.exc_text = self.formatException(record.exc_info)
            if record.exc_text:
                msg = f"{msg}\n{record.exc_text}"
            _pyre_emit_log(
                record.levelname,
                record.name,
                msg,
                record.pathname or "",
                record.lineno or 0,
                self._worker_id,
            )
        except Exception:
            pass  # Never crash business logic due to logging

_root = _logging.getLogger()
_root.handlers.clear()
_root.addHandler(_PyreRustHandler())
_root.setLevel(_logging.DEBUG)  # Let Rust EnvFilter do the real filtering

# -- Request / Response stubs ------------------------------------------------

class _PyreRequest:
    def __init__(self, method, path, params, query, body_bytes, headers):
        self.method = method
        self.path = path
        self.params = params
        self.query = query
        self.body_bytes = body_bytes
        self.headers = headers
    @property
    def body(self):
        return self.body_bytes
    @property
    def query_params(self):
        from urllib.parse import parse_qs
        return {k: v[0] for k, v in parse_qs(self.query).items()}
    def text(self):
        return self.body_bytes.decode('utf-8') if isinstance(self.body_bytes, bytes) else str(self.body_bytes)
    def json(self):
        import json
        return json.loads(self.text())

class _PyreResponse:
    def __init__(self, body="", status_code=200, content_type=None, headers=None):
        self.body = body
        self.status_code = status_code
        self.content_type = content_type
        self.headers = headers or {}

# -- Mock pyreframework modules ----------------------------------------------------

import sys, types, os
os.environ["PYRE_WORKER"] = "1"

_mock_engine = types.ModuleType("pyreframework.engine")
_mock_engine.PyreApp = type("PyreApp", (), {
    "__init__": lambda self: None,
    "get": lambda self, *a, **kw: (lambda f: f) if len(a) < 2 else None,
    "post": lambda self, *a, **kw: (lambda f: f) if len(a) < 2 else None,
    "put": lambda self, *a, **kw: (lambda f: f) if len(a) < 2 else None,
    "delete": lambda self, *a, **kw: (lambda f: f) if len(a) < 2 else None,
    "route": lambda self, *a, **kw: None,
    "before_request": lambda self, f: f,
    "after_request": lambda self, f: f,
    "fallback": lambda self, f: f,
    "websocket": lambda self, *a: (lambda f: f),
    "static_dir": lambda self, *a: None,
    "run": lambda self, **kw: None,
})
_mock_engine.PyreRequest = _PyreRequest
_mock_engine.PyreResponse = _PyreResponse
_mock_engine.PyreWebSocket = type("PyreWebSocket", (), {})
_mock_engine.SharedState = type("SharedState", (), {})
_mock_engine.PyreStream = type("PyreStream", (), {})
_mock_engine.get_gil_metrics = lambda: (0,0,0,0,0,0,0,0,0)

_mock_pyreframework = types.ModuleType("pyreframework")
_mock_pyreframework.engine = _mock_engine
_mock_pyreframework.PyreApp = _mock_engine.PyreApp
_mock_pyreframework.PyreRequest = _PyreRequest
_mock_pyreframework.PyreResponse = _PyreResponse
_mock_pyreframework.PyreWebSocket = _mock_engine.PyreWebSocket
_mock_pyreframework.SharedState = _mock_engine.SharedState
_mock_pyreframework.PyreStream = _mock_engine.PyreStream
_mock_pyreframework.get_gil_metrics = _mock_engine.get_gil_metrics
def _redirect(url, status_code=302):
    return _PyreResponse(body="", status_code=status_code, headers={"location": url})
_mock_pyreframework.redirect = _redirect

# Pyre wrapper (no-op in worker mode)
class _MockPyre:
    def __init__(self, debug=False, log_config=None): pass
    def get(self, path, handler=None, *, gil=False, model=None):
        if handler: return handler
        return lambda f: f
    def post(self, path, handler=None, *, gil=False, model=None):
        if handler: return handler
        return lambda f: f
    def put(self, path, handler=None, *, gil=False, model=None):
        if handler: return handler
        return lambda f: f
    def delete(self, path, handler=None, *, gil=False, model=None):
        if handler: return handler
        return lambda f: f
    def patch(self, path, handler=None, *, gil=False, model=None):
        if handler: return handler
        return lambda f: f
    def route(self, *a, **kw):
        return lambda f: f
    def before_request(self, f=None):
        return f if f else lambda fn: fn
    def after_request(self, f=None):
        return f if f else lambda fn: fn
    def fallback(self, f=None):
        return f if f else lambda fn: fn
    def rpc(self, path, **kw):
        return lambda f: f
    def websocket(self, path, handler=None):
        if handler: return handler
        return lambda f: f
    def static(self, *a): pass
    def enable_logging(self): pass
    def enable_cors(self, **kw): pass
    def run(self, **kw): pass
    @property
    def state(self):
        return {}
    @property
    def mcp(self):
        return type("MCP", (), {"tool": lambda s, *a, **kw: (lambda f: f), "resource": lambda s, *a, **kw: (lambda f: f), "prompt": lambda s, *a, **kw: (lambda f: f)})()

_mock_pyreframework.Pyre = _MockPyre

# App module mock
_mock_app = types.ModuleType("pyreframework.app")
_mock_app.Pyre = _MockPyre

sys.modules["pyreframework"] = _mock_pyreframework
sys.modules["pyreframework.engine"] = _mock_engine
sys.modules["pyreframework.app"] = _mock_app
sys.modules["pyreframework.mcp"] = types.ModuleType("pyreframework.mcp")

# -- Cookie utilities (pure Python) -------------------------------------------

_cookies_mod = types.ModuleType("pyreframework.cookies")
def _get_cookies(req):
    h = req.headers.get("cookie", "") if hasattr(req, "headers") else ""
    if not h: return {}
    r = {}
    for p in h.split(";"):
        p = p.strip()
        if "=" in p:
            n, _, v = p.partition("=")
            r[n.strip()] = v.strip()
    return r
def _get_cookie(req, name, default=None):
    return _get_cookies(req).get(name, default)
def _set_cookie(resp, name, value, **kw):
    parts = [f"{name}={value}"]
    if kw.get("max_age") is not None: parts.append(f"Max-Age={kw['max_age']}")
    if kw.get("path", "/"): parts.append(f"Path={kw.get('path','/')}")
    if kw.get("httponly"): parts.append("HttpOnly")
    if kw.get("secure"): parts.append("Secure")
    if kw.get("samesite", "Lax"): parts.append(f"SameSite={kw.get('samesite','Lax')}")
    hdrs = dict(getattr(resp, "headers", {}) or {})
    hdrs["set-cookie"] = "; ".join(parts)
    return _PyreResponse(body=resp.body, status_code=getattr(resp,"status_code",200), content_type=getattr(resp,"content_type",None), headers=hdrs)
def _delete_cookie(resp, name, **kw):
    return _set_cookie(resp, name, "", max_age=0, path=kw.get("path","/"))
_cookies_mod.get_cookies = _get_cookies
_cookies_mod.get_cookie = _get_cookie
_cookies_mod.set_cookie = _set_cookie
_cookies_mod.delete_cookie = _delete_cookie
sys.modules["pyreframework.cookies"] = _cookies_mod
sys.modules["pyreframework.rpc"] = types.ModuleType("pyreframework.rpc")
sys.modules["pyreframework.testing"] = types.ModuleType("pyreframework.testing")

# -- Pydantic stub (WARNING: replaces real pydantic in sub-interpreters) ------
# Pydantic V2's pydantic-core is a Rust/PyO3 extension that cannot load in
# sub-interpreters (no PEP 684 support yet). This stub lets user scripts that
# do `from pydantic import BaseModel` import without error. Real validation
# only runs on `gil=True` routes in the main interpreter.

class _FakeBaseModel:
    def __init_subclass__(cls, **kw): pass
    def __init__(self, **kw):
        for k, v in kw.items(): setattr(self, k, v)
    @classmethod
    def model_validate_json(cls, data): return cls()
    @classmethod
    def model_json_schema(cls): return {}
class _FakeField:
    def __init__(self, **kw): pass
    def __call__(self, **kw): return self
_pydantic_mod = types.ModuleType("pydantic")
_pydantic_mod.BaseModel = _FakeBaseModel
_pydantic_mod.Field = _FakeField(**{})
_pydantic_mod.field_validator = lambda *a, **kw: (lambda f: f)
sys.modules["pydantic"] = _pydantic_mod
# Also mock pydantic sub-modules that get imported
for _pm in ("pydantic.fields", "pydantic.main", "pydantic._migration",
            "pydantic.warnings", "pydantic.version", "pydantic_core"):
    sys.modules[_pm] = types.ModuleType(_pm)

# -- Upload utilities (pure Python) -------------------------------------------

_uploads_mod = types.ModuleType("pyreframework.uploads")
class _UploadFile:
    def __init__(self, name, filename, content_type, data):
        self.name = name
        self.filename = filename
        self.content_type = content_type
        self.data = data
    @property
    def text(self): return self.data.decode('utf-8', errors='replace')
    @property
    def size(self): return len(self.data)
def _parse_multipart(req):
    ct = req.headers.get("content-type", "")
    if "multipart/form-data" not in ct: raise ValueError("Not multipart")
    boundary = None
    for p in ct.split(";"):
        p = p.strip()
        if p.startswith("boundary="): boundary = p[9:].strip().strip('"')
    if not boundary: raise ValueError("No boundary")
    body = req.body if isinstance(req.body, bytes) else req.body.encode()
    parts = body.split(f"--{boundary}".encode())
    result = {}
    for part in parts:
        if not part or part.strip() in (b"--", b""): continue
        if b"\r\n\r\n" in part: hdr, data = part.split(b"\r\n\r\n", 1)
        elif b"\n\n" in part: hdr, data = part.split(b"\n\n", 1)
        else: continue
        if data.endswith(b"\r\n"): data = data[:-2]
        elif data.endswith(b"\n"): data = data[:-1]
        headers = {}
        for line in hdr.decode('utf-8', errors='replace').split("\n"):
            line = line.strip()
            if ":" in line:
                k, _, v = line.partition(":")
                headers[k.strip().lower()] = v.strip()
        disp = headers.get("content-disposition", "")
        fname = ffilename = None
        for pp in disp.split(";"):
            pp = pp.strip()
            if pp.startswith("name="): fname = pp[5:].strip('"')
            elif pp.startswith("filename="): ffilename = pp[9:].strip('"')
        if fname:
            ctype = headers.get("content-type", "application/octet-stream" if ffilename else "text/plain")
            result[fname] = _UploadFile(fname, ffilename, ctype, data)
    return result
_uploads_mod.parse_multipart = _parse_multipart
_uploads_mod.UploadFile = _UploadFile
sys.modules["pyreframework.uploads"] = _uploads_mod
