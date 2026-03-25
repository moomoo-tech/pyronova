"""Test: logging works in both GIL and sub-interpreter modes.

Verifies:
1. Framework enable_logging() produces structured output
2. User print() visible in sub-interpreter
3. User logging.info() visible in sub-interpreter
4. Rust-level sub-interp request logging works
"""

import subprocess
import sys
import os
import signal
import time

PYTHON = sys.executable


def run_server_and_check(script: str, label: str, expected_strings: list[str]):
    """Start a server, send requests, check output for expected strings."""
    script_path = f"/tmp/pyre_log_test_{label}.py"
    with open(script_path, "w") as f:
        f.write(script)

    proc = subprocess.Popen(
        [PYTHON, script_path],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        preexec_fn=os.setsid,
    )
    time.sleep(3)

    try:
        import urllib.request
        for _ in range(2):
            urllib.request.urlopen("http://127.0.0.1:9876/", timeout=2)
    except Exception:
        pass

    time.sleep(1)
    os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
    stdout, _ = proc.communicate(timeout=5)
    output = stdout.decode()

    passed = 0
    for s in expected_strings:
        if s in output:
            print(f"  ✅ {label}: found '{s}'")
            passed += 1
        else:
            print(f"  ❌ {label}: missing '{s}'")
            print(f"     output: {output[:500]}")

    return passed == len(expected_strings)


def test_gil_mode_logging():
    """Framework logging in GIL mode with timestamps."""
    script = '''
from pyreframework import Pyre
app = Pyre()
app.enable_logging()

@app.get("/")
def index(req): return "ok"

app.run(host="127.0.0.1", port=9876)
'''
    assert run_server_and_check(script, "gil_logging", [
        "[INFO ]",
        "GET /",
        "→ 200",
    ])


def test_subinterp_rust_logging():
    """Rust-level request logging in sub-interpreter mode."""
    script = '''
from pyreframework import Pyre
app = Pyre()
app.enable_logging()

@app.get("/")
def index(req): return "ok"

app.run(host="127.0.0.1", port=9876, mode="subinterp")
'''
    assert run_server_and_check(script, "subinterp_logging", [
        "[INFO ]",
        "GET /",
        "200",
    ])


def test_user_print_in_subinterp():
    """User print() works in sub-interpreter handlers."""
    script = '''
from pyreframework import Pyre
app = Pyre()

@app.get("/")
def index(req):
    print("USER_PRINT_MARKER", flush=True)
    return "ok"

app.run(host="127.0.0.1", port=9876, mode="subinterp")
'''
    assert run_server_and_check(script, "user_print", [
        "USER_PRINT_MARKER",
    ])


def test_user_logging_in_subinterp():
    """Python logging module works in sub-interpreter handlers."""
    script = '''
import logging
logging.basicConfig(level=logging.INFO, format="%(levelname)s %(message)s")
logger = logging.getLogger("test")

from pyreframework import Pyre
app = Pyre()

@app.get("/")
def index(req):
    logger.info("LOGGER_MARKER_12345")
    return "ok"

app.run(host="127.0.0.1", port=9876, mode="subinterp")
'''
    assert run_server_and_check(script, "user_logging", [
        "LOGGER_MARKER_12345",
    ])


if __name__ == "__main__":
    print("=== Logging Tests ===")
    print()
    test_gil_mode_logging()
    print()
    test_subinterp_rust_logging()
    print()
    test_user_print_in_subinterp()
    print()
    test_user_logging_in_subinterp()
    print()
    print("=== All logging tests passed ===")
