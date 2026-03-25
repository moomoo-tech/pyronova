"""Bootstrap script injected into each sub-interpreter worker.

Provides mock skytrade modules so user scripts can be executed in isolated
sub-interpreters without importing the real Rust extension (which doesn't
support PEP 684 multi-interpreter loading).

WARNING: This replaces `sys.modules["pydantic"]` with a no-op stub.
Pydantic validation is only available on routes with `gil=True`.
Sub-interpreter routes get a stub that lets `from pydantic import BaseModel`
succeed but does NOT perform real validation.
"""

# -- Request / Response stubs ------------------------------------------------

class _SkyRequest:
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

class _SkyResponse:
    def __init__(self, body="", status_code=200, content_type=None, headers=None):
        self.body = body
        self.status_code = status_code
        self.content_type = content_type
        self.headers = headers or {}

# -- Mock skytrade modules ----------------------------------------------------

import sys, types, os
os.environ["PYRE_WORKER"] = "1"

_mock_engine = types.ModuleType("skytrade.engine")
_mock_engine.SkyApp = type("SkyApp", (), {
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
_mock_engine.SkyRequest = _SkyRequest
_mock_engine.SkyResponse = _SkyResponse
_mock_engine.SkyWebSocket = type("SkyWebSocket", (), {})
_mock_engine.SharedState = type("SharedState", (), {})
_mock_engine.SkyStream = type("SkyStream", (), {})
_mock_engine.get_gil_metrics = lambda: (0,0,0,0,0,0,0,0,0)

_mock_skytrade = types.ModuleType("skytrade")
_mock_skytrade.engine = _mock_engine
_mock_skytrade.SkyApp = _mock_engine.SkyApp
_mock_skytrade.SkyRequest = _SkyRequest
_mock_skytrade.SkyResponse = _SkyResponse
_mock_skytrade.SkyWebSocket = _mock_engine.SkyWebSocket
_mock_skytrade.SharedState = _mock_engine.SharedState
_mock_skytrade.SkyStream = _mock_engine.SkyStream
_mock_skytrade.get_gil_metrics = _mock_engine.get_gil_metrics
def _redirect(url, status_code=302):
    return _SkyResponse(body="", status_code=status_code, headers={"location": url})
_mock_skytrade.redirect = _redirect

# Pyre wrapper (no-op in worker mode)
class _MockPyre:
    def __init__(self): pass
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

_mock_skytrade.Pyre = _MockPyre

# App module mock
_mock_app = types.ModuleType("skytrade.app")
_mock_app.Pyre = _MockPyre

sys.modules["skytrade"] = _mock_skytrade
sys.modules["skytrade.engine"] = _mock_engine
sys.modules["skytrade.app"] = _mock_app
sys.modules["skytrade.mcp"] = types.ModuleType("skytrade.mcp")

# -- Cookie utilities (pure Python) -------------------------------------------

_cookies_mod = types.ModuleType("skytrade.cookies")
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
    return _SkyResponse(body=resp.body, status_code=getattr(resp,"status_code",200), content_type=getattr(resp,"content_type",None), headers=hdrs)
def _delete_cookie(resp, name, **kw):
    return _set_cookie(resp, name, "", max_age=0, path=kw.get("path","/"))
_cookies_mod.get_cookies = _get_cookies
_cookies_mod.get_cookie = _get_cookie
_cookies_mod.set_cookie = _set_cookie
_cookies_mod.delete_cookie = _delete_cookie
sys.modules["skytrade.cookies"] = _cookies_mod
sys.modules["skytrade.rpc"] = types.ModuleType("skytrade.rpc")
sys.modules["skytrade.testing"] = types.ModuleType("skytrade.testing")

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

_uploads_mod = types.ModuleType("skytrade.uploads")
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
sys.modules["skytrade.uploads"] = _uploads_mod
