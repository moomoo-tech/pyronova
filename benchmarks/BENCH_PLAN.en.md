# Comprehensive Benchmark Plan

## Objective

Establish a **reusable automated benchmark framework** covering multiple workload types, comparing all competitors, and generating JSON data + HTML visualization reports.

## Comparison Matrix

| Framework | Mode | Description |
|-----------|------|-------------|
| Pyre SubInterp | `mode="subinterp"` | Sub-interpreters, 215k baseline |
| Pyre GIL | Default mode | Main interpreter + middleware |
| Pyre Hybrid | `mode="subinterp"` + `gil=True` routes | numpy routes go through GIL |
| Robyn --fast | Multi-process | Currently the fastest Python Rust framework |
| Axum (pure Rust) | No Python | Performance ceiling reference |

## Workload Scenarios

### Group 1: Basic Throughput

| ID | Scenario | Description | Measurement Target |
|----|----------|-------------|-------------------|
| T1 | Hello World | `return "Hello"` | Pure framework overhead |
| T2 | JSON small | `return {"key": "value"}` | JSON serialization |
| T3 | JSON medium | Nested dict with 10 fields | Medium serialization |
| T4 | JSON large | list[dict] with 1000 elements | Large JSON serialization stress |

### Group 2: CPU-bound

| ID | Scenario | Description | Measurement Target |
|----|----------|-------------|-------------------|
| C1 | fib(10) | Pure Python recursion | Light CPU |
| C2 | fib(20) | Pure Python recursion | Medium CPU |
| C3 | fib(30) | Pure Python recursion | Heavy CPU (blocking test) |

### Group 3: Python Ecosystem

| ID | Scenario | Description | Measurement Target |
|----|----------|-------------|-------------------|
| P1 | Pure Python math | `sum(range(10000))` | Pure Python computation |
| P2 | Python + numpy | `np.mean(np.random.randn(10000))` | numpy call overhead |
| P3 | Pure numpy heavy | `np.linalg.svd(100x100 matrix)` | numpy-intensive computation |

### Group 4: I/O Simulation

| ID | Scenario | Description | Measurement Target |
|----|----------|-------------|-------------------|
| I1 | sleep(1ms) | `asyncio.sleep(0.001)` or sync equivalent | Concurrency scheduling capability |
| I2 | File read | Read a 1KB file and return it | File I/O |

### Group 5: JSON Parsing

| ID | Scenario | Description | Measurement Target |
|----|----------|-------------|-------------------|
| J1 | Parse 100B JSON body | POST + json.loads | Small payload |
| J2 | Parse 10KB JSON body | POST + json.loads | Medium payload |
| J3 | Parse 100KB JSON body | POST + json.loads | Large payload |

## Benchmark Parameters

Each scenario runs with uniform settings:
- **Warmup**: wrk -t2 -c50 -d3s (results discarded)
- **Main run**: wrk -t4 -c256 -d10s (results recorded)
- **Light load**: wrk -t1 -c10 -d10s (low-concurrency latency)

## Toolchain

```
benchmarks/
├── BENCH_PLAN.md           ← This file
├── suite/
│   ├── runner.py           ← Main controller: start servers, run wrk, collect results
│   ├── servers/
│   │   ├── pyre_subinterp.py
│   │   ├── pyre_gil.py
│   │   ├── pyre_hybrid.py
│   │   ├── robyn_server.py
│   │   └── axum_server/    ← Cargo project, pure Rust baseline
│   ├── payloads/
│   │   ├── small.json      ← 100B
│   │   ├── medium.json     ← 10KB
│   │   └── large.json      ← 100KB
│   └── report/
│       ├── template.html   ← Jinja2 HTML report template
│       └── charts.py       ← matplotlib chart generation
├── results/
│   └── YYYY-MM-DD_HHMMSS/
│       ├── raw.json        ← All raw data
│       ├── summary.md      ← Markdown summary
│       ├── report.html     ← Visualization report
│       └── charts/
│           ├── throughput.png
│           ├── latency.png
│           └── by_workload.png
```

## runner.py Core Flow

```python
for framework in [pyre_subinterp, pyre_gil, pyre_hybrid, robyn, axum]:
    start_server(framework, port)
    wait_ready(port)
    for scenario in [T1, T2, ..., J3]:
        # Warmup
        run_wrk(warmup_params, port, scenario.path)
        # Main test
        result = run_wrk(bench_params, port, scenario.path)
        # POST scenarios use wrk -s post.lua
        results.append({
            "framework": framework.name,
            "scenario": scenario.id,
            "req_per_sec": result.rps,
            "avg_latency_ms": result.latency,
            "p99_latency_ms": result.p99,
            "errors": result.errors,
        })
    stop_server(framework)

save_json(results)
generate_charts(results)
generate_report(results)
```

## Visualization

1. **Grouped bar chart**: X-axis = scenario, Y-axis = req/s, 5 bars per group (5 frameworks)
2. **Latency comparison**: X-axis = scenario, Y-axis = avg latency (ms), same layout
3. **Radar chart**: One line per framework, 6 dimensions (throughput, latency, CPU-bound, I/O, JSON, numpy)
4. **Heatmap**: Framework × scenario, color = req/s

## Usage

```bash
# Run all tests and generate report
python benchmarks/suite/runner.py

# Run only a specific framework
python benchmarks/suite/runner.py --framework pyre_subinterp

# Run only a specific scenario group
python benchmarks/suite/runner.py --group cpu

# Compare two runs
python benchmarks/suite/runner.py --compare results/2026-03-24_120000 results/2026-03-25_120000
```

## Implementation Order

1. Create all server scripts (one file per framework, unified routes)
2. Generate JSON payload files
3. Write `runner.py` main controller + wrk result parsing
4. Write `charts.py` chart generation
5. Write HTML report template
6. Run the first round of comprehensive tests
