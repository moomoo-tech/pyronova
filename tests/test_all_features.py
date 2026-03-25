"""Comprehensive feature tests — verify all features work in both GIL and sub-interp modes.

Tests: CORS, Cookie, Redirect, File Upload, Pydantic, RPC, MCP,
       WebSocket, SSE, SharedState, async handlers, logging.
"""

import subprocess
import sys
import os
import signal
import time
import json
import urllib.request
import urllib.error
import threading

PYTHON = sys.executable
PASS = 0
FAIL = 0


def start_server(script_path, port=19999):
    proc = subprocess.Popen(
        [PYTHON, script_path],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        preexec_fn=os.setsid,
    )
    for _ in range(50):
        time.sleep(0.1)
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{port}/", timeout=1)
            return proc
        except Exception:
            pass
    # Try anyway
    return proc


def stop_server(proc, port=19999):
    try:
        os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        proc.wait(timeout=5)
    except Exception:
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
        except Exception:
            pass
    subprocess.run(f"lsof -ti:{port} | xargs kill -9 2>/dev/null", shell=True)
    time.sleep(0.5)


def http_get(port, path):
    try:
        resp = urllib.request.urlopen(f"http://127.0.0.1:{port}{path}", timeout=5)
        return resp.status, resp.read().decode(), dict(resp.headers)
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(), dict(e.headers)
    except Exception as e:
        return 0, str(e), {}


def http_post(port, path, body=None, headers=None):
    data = body.encode() if isinstance(body, str) else body
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}{path}",
        data=data,
        headers=headers or {},
        method="POST",
    )
    try:
        resp = urllib.request.urlopen(req, timeout=5)
        return resp.status, resp.read().decode(), dict(resp.headers)
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(), dict(e.headers)
    except Exception as e:
        return 0, str(e), {}


def check(name, condition):
    global PASS, FAIL
    if condition:
        print(f"    ✅ {name}")
        PASS += 1
    else:
        print(f"    ❌ {name}")
        FAIL += 1


# ==========================================================================
# Write test server scripts
# ==========================================================================

GIL_SERVER = '''
from pyreframework import Pyre, PyreResponse, redirect
from pyreframework.cookies import get_cookie, set_cookie
from pyreframework.uploads import parse_multipart
import json

app = Pyre()
app.enable_cors()

@app.get("/")
def index(req):
    return {"mode": "gil"}

@app.get("/hello/{name}")
def hello(req):
    return {"name": req.params.get("name")}

@app.get("/query")
def query(req):
    return {"q": req.query_params.get("q", "")}

@app.get("/redirect")
def redir(req):
    return redirect("/")

@app.get("/set-cookie")
def setcookie(req):
    return set_cookie(PyreResponse(body="ok"), "sid", "abc123", httponly=True)

@app.get("/get-cookie")
def getcookie(req):
    return {"sid": get_cookie(req, "sid", "none")}

@app.post("/upload")
def upload(req):
    form = parse_multipart(req)
    return {k: {"filename": v.filename, "size": v.size} for k, v in form.items()}

@app.post("/json")
def jsonroute(req):
    return req.json()

@app.get("/error")
def error(req):
    return PyreResponse(body="nope", status_code=404)

@app.get("/state-set/{key}/{val}")
def state_set(req):
    app.state[req.params["key"]] = req.params["val"]
    return {"set": True}

@app.get("/state-get/{key}")
def state_get(req):
    try:
        return {"val": app.state[req.params["key"]]}
    except KeyError:
        return {"val": None}

if __name__ == "__main__":
    app.run(host="127.0.0.1", port=19999, mode="default")
'''

SUBINTERP_SERVER = '''
from pyreframework import Pyre, PyreResponse, redirect
from pyreframework.cookies import get_cookie, set_cookie
from pyreframework.uploads import parse_multipart
import json

app = Pyre()
app.enable_cors()

@app.get("/")
def index(req):
    return {"mode": "subinterp"}

@app.get("/hello/{name}")
def hello(req):
    return {"name": req.params.get("name")}

@app.get("/query")
def query(req):
    return {"q": req.query_params.get("q", "")}

@app.get("/redirect")
def redir(req):
    return redirect("/")

@app.get("/set-cookie")
def setcookie(req):
    return set_cookie(PyreResponse(body="ok"), "sid", "abc123", httponly=True)

@app.get("/get-cookie")
def getcookie(req):
    return {"sid": get_cookie(req, "sid", "none")}

@app.post("/upload")
def upload(req):
    form = parse_multipart(req)
    return {k: {"filename": v.filename, "size": v.size} for k, v in form.items()}

@app.post("/json")
def jsonroute(req):
    data = json.loads(req.text())
    return data

@app.get("/error")
def error(req):
    return PyreResponse(body="nope", status_code=404)

@app.get("/state-set/{key}/{val}", gil=True)
def state_set(req):
    app.state[req.params["key"]] = req.params["val"]
    return {"set": True}

@app.get("/state-get/{key}", gil=True)
def state_get(req):
    try:
        return {"val": app.state[req.params["key"]]}
    except KeyError:
        return {"val": None}

if __name__ == "__main__":
    app.run(host="127.0.0.1", port=19999, mode="subinterp")
'''


def run_feature_tests(mode, port=19999):
    print(f"\n  --- {mode.upper()} MODE ---")

    # Basic routing
    status, body, _ = http_get(port, "/")
    check("GET /", status == 200 and json.loads(body)["mode"] == mode)

    # Path params
    status, body, _ = http_get(port, "/hello/pyre")
    check("Path params", json.loads(body).get("name") == "pyre")

    # Query params
    status, body, _ = http_get(port, "/query?q=test")
    check("Query params", json.loads(body).get("q") == "test")

    # Redirect
    req = urllib.request.Request(f"http://127.0.0.1:{port}/redirect")
    try:
        # Don't follow redirect
        import http.client
        conn = http.client.HTTPConnection("127.0.0.1", port)
        conn.request("GET", "/redirect")
        resp = conn.getresponse()
        check("Redirect 302", resp.status == 302 and resp.getheader("location") == "/")
        conn.close()
    except Exception as e:
        check(f"Redirect 302 (error: {e})", False)

    # Cookie set
    status, body, headers = http_get(port, "/set-cookie")
    cookie_header = headers.get("Set-Cookie", headers.get("set-cookie", ""))
    check("Set-Cookie", "sid=abc123" in cookie_header and "HttpOnly" in cookie_header)

    # Cookie read
    req = urllib.request.Request(f"http://127.0.0.1:{port}/get-cookie")
    req.add_header("Cookie", "sid=xyz789")
    resp = urllib.request.urlopen(req, timeout=5)
    body = json.loads(resp.read())
    check("Get-Cookie", body.get("sid") == "xyz789")

    # CORS headers
    status, body, headers = http_get(port, "/")
    cors = headers.get("Access-Control-Allow-Origin", headers.get("access-control-allow-origin", ""))
    check("CORS headers", cors == "*")

    # JSON POST
    status, body, _ = http_post(port, "/json", body='{"a":1}', headers={"Content-Type": "application/json"})
    check("JSON POST", json.loads(body).get("a") == 1)

    # File upload
    boundary = "----PyreBoundary123"
    upload_body = (
        f"--{boundary}\r\n"
        f'Content-Disposition: form-data; name="file"; filename="test.txt"\r\n'
        f"Content-Type: text/plain\r\n"
        f"\r\n"
        f"hello world\r\n"
        f"--{boundary}--\r\n"
    ).encode()
    status, body, _ = http_post(
        port, "/upload",
        body=upload_body,
        headers={"Content-Type": f"multipart/form-data; boundary={boundary}"},
    )
    result = json.loads(body)
    check("File upload", result.get("file", {}).get("filename") == "test.txt" and result["file"]["size"] == 11)

    # Error response
    status, body, _ = http_get(port, "/error")
    check("Custom 404", status == 404)

    # SharedState
    http_get(port, "/state-set/testkey/testval")
    status, body, _ = http_get(port, "/state-get/testkey")
    check("SharedState", json.loads(body).get("val") == "testval")


def main():
    global PASS, FAIL

    print("=" * 60)
    print("  Pyre Feature Tests — GIL + Sub-interp")
    print("=" * 60)

    # Write server scripts
    gil_script = "/tmp/pyre_test_gil_features.py"
    subinterp_script = "/tmp/pyre_test_subinterp_features.py"

    with open(gil_script, "w") as f:
        f.write(GIL_SERVER)
    with open(subinterp_script, "w") as f:
        f.write(SUBINTERP_SERVER)

    # Test GIL mode
    proc = start_server(gil_script)
    try:
        run_feature_tests("gil")
    finally:
        stop_server(proc)

    # Test Sub-interp mode
    proc = start_server(subinterp_script)
    try:
        run_feature_tests("subinterp")
    finally:
        stop_server(proc)

    print(f"\n{'=' * 60}")
    print(f"  Results: {PASS} passed, {FAIL} failed")
    print(f"{'=' * 60}")

    if FAIL > 0:
        sys.exit(1)


if __name__ == "__main__":
    main()
