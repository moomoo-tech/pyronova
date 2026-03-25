"""Tests for WebSocket support."""

import asyncio
import json
import subprocess
import sys
import os
import signal
import time
import pytest

PYTHON = sys.executable

WS_SERVER = '''
from skytrade import Pyre

app = Pyre()

@app.websocket("/echo")
def echo(ws):
    while True:
        msg = ws.recv()
        if msg is None:
            break
        ws.send(f"echo: {msg}")

@app.websocket("/json")
def json_ws(ws):
    import json
    while True:
        msg = ws.recv()
        if msg is None:
            break
        data = json.loads(msg)
        data["server"] = True
        ws.send(json.dumps(data))

@app.get("/health")
def health(req):
    return {"ok": True}

if __name__ == "__main__":
    app.run(host="127.0.0.1", port=19880, mode="default")
'''


@pytest.fixture(scope="module")
def ws_server():
    """Start a WebSocket-capable server as a subprocess."""
    script = "/tmp/pyre_ws_test_server.py"
    with open(script, "w") as f:
        f.write(WS_SERVER)

    proc = subprocess.Popen(
        [PYTHON, script],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        preexec_fn=os.setsid,
    )
    # Wait for server
    import urllib.request
    for _ in range(50):
        time.sleep(0.1)
        try:
            urllib.request.urlopen("http://127.0.0.1:19880/health", timeout=1)
            break
        except Exception:
            pass

    yield proc

    try:
        os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
        proc.wait(timeout=5)
    except Exception:
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
        except Exception:
            pass


@pytest.mark.asyncio
async def test_websocket_echo(ws_server):
    import websockets
    async with websockets.connect("ws://127.0.0.1:19880/echo") as ws:
        await ws.send("hello")
        resp = await asyncio.wait_for(ws.recv(), timeout=5)
        assert resp == "echo: hello"

        await ws.send("world")
        resp = await asyncio.wait_for(ws.recv(), timeout=5)
        assert resp == "echo: world"


@pytest.mark.asyncio
async def test_websocket_json(ws_server):
    import websockets
    async with websockets.connect("ws://127.0.0.1:19880/json") as ws:
        await ws.send(json.dumps({"key": "value"}))
        resp = await asyncio.wait_for(ws.recv(), timeout=5)
        data = json.loads(resp)
        assert data["key"] == "value"
        assert data["server"] is True


@pytest.mark.asyncio
async def test_websocket_multiple_messages(ws_server):
    import websockets
    async with websockets.connect("ws://127.0.0.1:19880/echo") as ws:
        for i in range(10):
            await ws.send(f"msg-{i}")
            resp = await asyncio.wait_for(ws.recv(), timeout=5)
            assert resp == f"echo: msg-{i}"
