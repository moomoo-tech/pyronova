"""Shared pytest fixtures for Pyre integration tests.

Provides a parameterised `feature_server` fixture that spins up Pyre in
either GIL or sub-interpreter mode on an ephemeral port, yields a base
URL, and tears down cleanly. Individual tests express their routes via
the SERVER_SCRIPT string (a template) and reuse the fixture.

The old test_all_features.py used one giant `run_feature_tests()` that
bundled 13+ assertions under one server; splitting by topic means each
file pays its own startup cost but is independently runnable and
failure-isolated.
"""

from __future__ import annotations

import json
import os
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass

import pytest

HOST = "127.0.0.1"


def _free_port() -> int:
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


@dataclass
class ServerHandle:
    base_url: str
    mode: str
    proc: subprocess.Popen

    def get(self, path: str, headers: dict | None = None) -> tuple[int, str, dict]:
        req = urllib.request.Request(self.base_url + path, headers=headers or {})
        try:
            resp = urllib.request.urlopen(req, timeout=5)
            return resp.status, resp.read().decode(), dict(resp.headers)
        except urllib.error.HTTPError as e:
            return e.code, e.read().decode(), dict(e.headers)

    def post(
        self, path: str, body: bytes | str | None = None,
        headers: dict | None = None,
    ) -> tuple[int, str, dict]:
        data = body.encode() if isinstance(body, str) else body
        req = urllib.request.Request(
            self.base_url + path, data=data, headers=headers or {}, method="POST",
        )
        try:
            resp = urllib.request.urlopen(req, timeout=5)
            return resp.status, resp.read().decode(), dict(resp.headers)
        except urllib.error.HTTPError as e:
            return e.code, e.read().decode(), dict(e.headers)


def _boot(script: str, mode: str, port: int) -> subprocess.Popen:
    """Start a Pyre server from a script string. Returns the process handle.
    `mode` controls whether app.run() uses subinterp or GIL mode — the
    script is expected to read $PYRE_MODE and branch.
    """
    path = f"/tmp/pyre_test_{os.getpid()}_{port}.py"
    with open(path, "w") as f:
        f.write(script)
    env = dict(os.environ)
    env["PYRE_MODE"] = mode
    env["PYRE_PORT"] = str(port)
    proc = subprocess.Popen(
        [sys.executable, path],
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        preexec_fn=os.setsid, env=env,
    )
    # Poll until responsive
    deadline = time.time() + 10
    last_err = None
    while time.time() < deadline:
        try:
            urllib.request.urlopen(f"http://{HOST}:{port}/__ping", timeout=0.5)
            return proc
        except Exception as e:  # noqa: BLE001 — we only care it starts
            last_err = e
            time.sleep(0.1)
    # Failed to start — harvest stderr for the error message
    proc.kill()
    out, _ = proc.communicate(timeout=5)
    raise RuntimeError(
        f"Pyre server ({mode} mode on port {port}) failed to start: "
        f"{last_err}\nServer output:\n{out.decode(errors='replace')[:2000]}"
    )


def _teardown(proc: subprocess.Popen) -> None:
    try:
        os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
        proc.wait(timeout=5)
    except Exception:
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
        except Exception:
            pass


def feature_server_factory(script: str):
    """Build a parametrised pytest fixture that runs `script` in both
    GIL and sub-interp mode. Scope is module to amortise startup cost
    across a whole file.

    Usage:
      from tests.conftest import feature_server_factory
      feature_server = feature_server_factory(SERVER_SCRIPT)
    """

    @pytest.fixture(scope="module", params=["gil", "subinterp"])
    def feature_server(request):  # type: ignore[misc]
        port = _free_port()
        proc = _boot(script, request.param, port)
        try:
            yield ServerHandle(
                base_url=f"http://{HOST}:{port}",
                mode=request.param,
                proc=proc,
            )
        finally:
            _teardown(proc)

    return feature_server
