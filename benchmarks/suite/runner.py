#!/usr/bin/env python3
"""Pyre Benchmark Suite — automated multi-framework, multi-scenario benchmarking."""

import json
import os
import re
import signal
import subprocess
import sys
import time
from dataclasses import dataclass, field, asdict
from datetime import datetime
from pathlib import Path

# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------

PYTHON = sys.executable
BASE_DIR = Path(__file__).parent
PAYLOADS_DIR = BASE_DIR / "payloads"
RESULTS_DIR = BASE_DIR.parent / "results"
PORT = 9000
WRK = "wrk"

WARMUP = "-t2 -c50 -d3s"
BENCH_HIGH = "-t4 -c256 -d10s"
BENCH_LOW = "-t1 -c10 -d10s"


@dataclass
class Scenario:
    id: str
    name: str
    path: str
    method: str = "GET"
    payload_file: str | None = None
    group: str = "basic"
    needs_numpy: bool = False


SCENARIOS = [
    # Group 1: Basic throughput
    Scenario("t1", "Hello World", "/t1", group="basic"),
    Scenario("t2", "JSON small (3 fields)", "/t2", group="basic"),
    Scenario("t3", "JSON medium (100 users)", "/t3", group="basic"),
    Scenario("t4", "JSON large (500 records)", "/t4", group="basic"),
    # Group 2: CPU
    Scenario("c1", "fib(10)", "/c1", group="cpu"),
    Scenario("c2", "fib(20)", "/c2", group="cpu"),
    Scenario("c3", "fib(30)", "/c3", group="cpu"),
    # Group 3: Python ecosystem
    Scenario("p1", "Pure Python sum(10k)", "/p1", group="python"),
    Scenario("p2", "numpy mean(10k)", "/p2", group="python", needs_numpy=True),
    Scenario("p3", "numpy SVD 100x100", "/p3", group="python", needs_numpy=True),
    # Group 4: I/O
    Scenario("i1", "sleep(1ms)", "/i1", group="io"),
    # Group 5: JSON parsing
    Scenario("j1", "Parse 41B JSON", "/j1", method="POST", payload_file="small.json", group="json"),
    Scenario("j2", "Parse 7KB JSON", "/j2", method="POST", payload_file="medium.json", group="json"),
    Scenario("j3", "Parse 93KB JSON", "/j3", method="POST", payload_file="large.json", group="json"),
]


@dataclass
class Framework:
    name: str
    script: str
    extra_args: list = field(default_factory=list)
    supports_numpy: bool = False
    startup_wait: float = 2.0


FRAMEWORKS = [
    Framework("pyre_subinterp", "servers/pyre_subinterp.py", supports_numpy=False),
    Framework("pyre_gil", "servers/pyre_gil.py", supports_numpy=True),
    Framework("pyre_hybrid", "servers/pyre_hybrid.py", supports_numpy=True),
    Framework("robyn", "servers/robyn_server.py", extra_args=["--fast"], supports_numpy=True, startup_wait=3.0),
]


# ---------------------------------------------------------------------------
# wrk result parser
# ---------------------------------------------------------------------------

def parse_wrk_output(output: str) -> dict:
    """Parse wrk output into structured data."""
    result = {
        "req_per_sec": 0.0,
        "avg_latency_ms": 0.0,
        "max_latency_ms": 0.0,
        "stdev_latency_ms": 0.0,
        "total_requests": 0,
        "errors": 0,
        "transfer_per_sec": "",
    }

    # Requests/sec: 215428.89
    m = re.search(r"Requests/sec:\s+([\d.]+)", output)
    if m:
        result["req_per_sec"] = float(m.group(1))

    # Latency   836.85us  561.33us  18.81ms   89.38%
    m = re.search(r"Latency\s+([\d.]+)(us|ms|s)\s+([\d.]+)(us|ms|s)\s+([\d.]+)(us|ms|s)", output)
    if m:
        def to_ms(val, unit):
            v = float(val)
            if unit == "us": return v / 1000
            if unit == "s": return v * 1000
            return v
        result["avg_latency_ms"] = round(to_ms(m.group(1), m.group(2)), 3)
        result["stdev_latency_ms"] = round(to_ms(m.group(3), m.group(4)), 3)
        result["max_latency_ms"] = round(to_ms(m.group(5), m.group(6)), 3)

    # 2157773 requests in 10.02s
    m = re.search(r"(\d+) requests in", output)
    if m:
        result["total_requests"] = int(m.group(1))

    # Socket errors or Non-2xx
    m = re.search(r"Socket errors.*?(\d+)", output)
    if m:
        result["errors"] += int(m.group(1))
    m = re.search(r"Non-2xx or 3xx responses:\s+(\d+)", output)
    if m:
        result["errors"] += int(m.group(1))

    # Transfer/sec
    m = re.search(r"Transfer/sec:\s+(.+)", output)
    if m:
        result["transfer_per_sec"] = m.group(1).strip()

    return result


# ---------------------------------------------------------------------------
# Runner
# ---------------------------------------------------------------------------

def run_wrk(params: str, url: str, lua_script: str | None = None, env: dict | None = None) -> str:
    cmd = f"{WRK} {params} {url}"
    if lua_script:
        cmd += f" -s {lua_script}"
    proc_env = {**os.environ, **(env or {})}
    result = subprocess.run(cmd, shell=True, capture_output=True, text=True, env=proc_env, timeout=120)
    return result.stdout + result.stderr


def start_server(framework: Framework) -> subprocess.Popen:
    script_path = str(BASE_DIR / framework.script)
    cmd = [PYTHON, script_path] + framework.extra_args
    proc = subprocess.Popen(
        cmd,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        preexec_fn=os.setsid,
    )
    time.sleep(framework.startup_wait)
    return proc


def stop_server(proc: subprocess.Popen):
    try:
        os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
    # Extra cleanup
    subprocess.run(f"lsof -ti:{PORT} | xargs kill -9 2>/dev/null", shell=True)
    time.sleep(1)


def check_server(port: int) -> bool:
    import urllib.request
    try:
        urllib.request.urlopen(f"http://127.0.0.1:{port}/t1", timeout=2)
        return True
    except Exception:
        return False


def run_benchmark():
    timestamp = datetime.now().strftime("%Y-%m-%d_%H%M%S")
    result_dir = RESULTS_DIR / timestamp
    result_dir.mkdir(parents=True, exist_ok=True)

    all_results = []
    lua_script = str(BASE_DIR / "post.lua")

    total_tests = sum(
        1 for fw in FRAMEWORKS
        for sc in SCENARIOS
        if not sc.needs_numpy or fw.supports_numpy
    )
    completed = 0

    print(f"\n{'='*70}")
    print(f"  Pyre Benchmark Suite — {timestamp}")
    print(f"  {len(FRAMEWORKS)} frameworks × {len(SCENARIOS)} scenarios")
    print(f"  Total tests: {total_tests}")
    print(f"{'='*70}\n")

    for fw in FRAMEWORKS:
        print(f"\n>>> Starting {fw.name}...")
        proc = start_server(fw)

        if not check_server(PORT):
            print(f"  ✗ {fw.name} failed to start, skipping")
            stop_server(proc)
            continue

        print(f"  ✓ {fw.name} ready on :{PORT}")

        for sc in SCENARIOS:
            if sc.needs_numpy and not fw.supports_numpy:
                continue

            completed += 1
            url = f"http://127.0.0.1:{PORT}{sc.path}"
            label = f"  [{completed}/{total_tests}] {fw.name} / {sc.id} ({sc.name})"
            print(f"{label}...", end="", flush=True)

            env = {}
            use_lua = None
            if sc.method == "POST" and sc.payload_file:
                env["WRK_BODY_FILE"] = str(PAYLOADS_DIR / sc.payload_file)
                use_lua = lua_script

            # Warmup
            run_wrk(WARMUP, url, lua_script=use_lua, env=env)

            # High concurrency
            output_high = run_wrk(BENCH_HIGH, url, lua_script=use_lua, env=env)
            result_high = parse_wrk_output(output_high)

            # Low concurrency
            output_low = run_wrk(BENCH_LOW, url, lua_script=use_lua, env=env)
            result_low = parse_wrk_output(output_low)

            rps = result_high["req_per_sec"]
            lat = result_high["avg_latency_ms"]
            print(f" {rps:,.0f} req/s, {lat:.2f}ms")

            all_results.append({
                "framework": fw.name,
                "scenario_id": sc.id,
                "scenario_name": sc.name,
                "group": sc.group,
                "high_concurrency": result_high,
                "low_concurrency": result_low,
            })

        stop_server(proc)
        print(f"  ✓ {fw.name} stopped")

    # Save results
    raw_path = result_dir / "raw.json"
    with open(raw_path, "w") as f:
        json.dump({
            "timestamp": timestamp,
            "system": {
                "python": sys.version,
                "wrk_params_high": BENCH_HIGH,
                "wrk_params_low": BENCH_LOW,
            },
            "results": all_results,
        }, f, indent=2)

    print(f"\n  Results saved: {raw_path}")

    # Generate summary
    generate_summary(all_results, result_dir)

    return str(result_dir)


def generate_summary(results: list, result_dir: Path):
    """Generate markdown summary."""
    lines = [
        "# Benchmark Summary\n",
        f"Generated: {datetime.now().isoformat()}\n",
    ]

    # Group by scenario
    groups = {}
    for r in results:
        gid = r["group"]
        if gid not in groups:
            groups[gid] = []
        groups[gid].append(r)

    group_names = {
        "basic": "Basic Throughput",
        "cpu": "CPU Intensive",
        "python": "Python Ecosystem",
        "io": "I/O Simulation",
        "json": "JSON Parsing",
    }

    for gid, gname in group_names.items():
        if gid not in groups:
            continue
        lines.append(f"\n## {gname}\n")
        lines.append("| Scenario | " + " | ".join(
            f"{r['framework']}" for r in groups[gid] if r["scenario_id"] == groups[gid][0]["scenario_id"]
        ) + " |")

        # Collect scenario ids
        sids = []
        for r in groups[gid]:
            if r["scenario_id"] not in sids:
                sids.append(r["scenario_id"])

        lines.append("|" + "---|" * (1 + len(set(r["framework"] for r in groups[gid]))))

        for sid in sids:
            scenario_results = [r for r in groups[gid] if r["scenario_id"] == sid]
            name = scenario_results[0]["scenario_name"]
            cells = []
            for sr in scenario_results:
                rps = sr["high_concurrency"]["req_per_sec"]
                lat = sr["high_concurrency"]["avg_latency_ms"]
                cells.append(f"{rps:,.0f} ({lat:.1f}ms)")
            lines.append(f"| {name} | " + " | ".join(cells) + " |")

    # Write markdown
    summary_path = result_dir / "summary.md"
    with open(summary_path, "w") as f:
        f.write("\n".join(lines))

    print(f"  Summary saved: {summary_path}")

    # Also generate a quick comparison table
    print(f"\n{'='*70}")
    print("  HIGH CONCURRENCY RESULTS (wrk -t4 -c256 -d10s)")
    print(f"{'='*70}")
    print(f"\n  {'Scenario':<25} ", end="")
    fw_names = sorted(set(r["framework"] for r in results))
    for fw in fw_names:
        print(f"{fw:>18}", end="")
    print()
    print("  " + "-" * (25 + 18 * len(fw_names)))

    sids_seen = []
    for r in results:
        sid = r["scenario_id"]
        if sid in sids_seen:
            continue
        sids_seen.append(sid)
        print(f"  {r['scenario_name']:<25} ", end="")
        for fw in fw_names:
            match = [x for x in results if x["framework"] == fw and x["scenario_id"] == sid]
            if match:
                rps = match[0]["high_concurrency"]["req_per_sec"]
                print(f"{rps:>15,.0f} r/s", end="")
            else:
                print(f"{'—':>18}", end="")
        print()


if __name__ == "__main__":
    run_benchmark()
