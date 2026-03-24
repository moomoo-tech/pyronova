# 全面压测计划

## 目标

建立一套**可复用的自动化 benchmark 框架**，覆盖多种负载类型，对比所有竞品，生成 JSON 数据 + HTML 可视化报告。

## 对比矩阵

| 框架 | 模式 | 说明 |
|------|------|------|
| Pyre SubInterp | `mode="subinterp"` | 子解释器，215k baseline |
| Pyre GIL | 默认模式 | 主解释器 + middleware |
| Pyre Hybrid | `mode="subinterp"` + `gil=True` 路由 | numpy 路由走 GIL |
| Robyn --fast | 多进程 | 当前最快的 Python Rust 框架 |
| Axum (纯 Rust) | 无 Python | 性能天花板参考线 |

## 负载场景

### Group 1: 基础吞吐

| ID | 场景 | 说明 | 测量目标 |
|----|------|------|---------|
| T1 | Hello World | `return "Hello"` | 框架纯开销 |
| T2 | JSON small | `return {"key": "value"}` | JSON 序列化 |
| T3 | JSON medium | 10 个字段的嵌套 dict | 中等序列化 |
| T4 | JSON large | 1000 个元素的 list[dict] | 大 JSON 序列化压力 |

### Group 2: CPU 密集

| ID | 场景 | 说明 | 测量目标 |
|----|------|------|---------|
| C1 | fib(10) | 纯 Python 递归 | 轻 CPU |
| C2 | fib(20) | 纯 Python 递归 | 中 CPU |
| C3 | fib(30) | 纯 Python 递归 | 重 CPU（阻塞测试） |

### Group 3: Python 生态

| ID | 场景 | 说明 | 测量目标 |
|----|------|------|---------|
| P1 | Pure Python math | `sum(range(10000))` | 纯 Python 计算 |
| P2 | Python + numpy | `np.mean(np.random.randn(10000))` | numpy 调用开销 |
| P3 | Pure numpy heavy | `np.linalg.svd(100x100 matrix)` | numpy 密集计算 |

### Group 4: I/O 模拟

| ID | 场景 | 说明 | 测量目标 |
|----|------|------|---------|
| I1 | sleep(1ms) | `asyncio.sleep(0.001)` 或同步等价 | 并发调度能力 |
| I2 | 读文件 | 读一个 1KB 文件返回 | 文件 I/O |

### Group 5: JSON 解析

| ID | 场景 | 说明 | 测量目标 |
|----|------|------|---------|
| J1 | Parse 100B JSON body | POST + json.loads | 小 payload |
| J2 | Parse 10KB JSON body | POST + json.loads | 中 payload |
| J3 | Parse 100KB JSON body | POST + json.loads | 大 payload |

## 压测参数

每个场景统一运行：
- **预热**: wrk -t2 -c50 -d3s (丢弃结果)
- **正式**: wrk -t4 -c256 -d10s (记录结果)
- **轻负载**: wrk -t1 -c10 -d10s (低并发延迟)

## 工具链

```
benchmarks/
├── BENCH_PLAN.md           ← 本文件
├── suite/
│   ├── runner.py           ← 主控脚本：启动服务、跑 wrk、收集结果
│   ├── servers/
│   │   ├── pyre_subinterp.py
│   │   ├── pyre_gil.py
│   │   ├── pyre_hybrid.py
│   │   ├── robyn_server.py
│   │   └── axum_server/    ← Cargo 项目，纯 Rust 对照
│   ├── payloads/
│   │   ├── small.json      ← 100B
│   │   ├── medium.json     ← 10KB
│   │   └── large.json      ← 100KB
│   └── report/
│       ├── template.html   ← Jinja2 HTML 报告模板
│       └── charts.py       ← matplotlib 图表生成
├── results/
│   └── YYYY-MM-DD_HHMMSS/
│       ├── raw.json        ← 全部原始数据
│       ├── summary.md      ← Markdown 摘要
│       ├── report.html     ← 可视化报告
│       └── charts/
│           ├── throughput.png
│           ├── latency.png
│           └── by_workload.png
```

## runner.py 核心流程

```python
for framework in [pyre_subinterp, pyre_gil, pyre_hybrid, robyn, axum]:
    start_server(framework, port)
    wait_ready(port)
    for scenario in [T1, T2, ..., J3]:
        # 预热
        run_wrk(warmup_params, port, scenario.path)
        # 正式测试
        result = run_wrk(bench_params, port, scenario.path)
        # POST 场景用 wrk -s post.lua
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

## 可视化

1. **分组柱状图**: X 轴=场景，Y 轴=req/s，每组 5 根柱子（5 个框架）
2. **延迟对比**: X 轴=场景，Y 轴=avg latency (ms)，同上
3. **雷达图**: 每个框架一条线，6 个维度（吞吐、延迟、CPU 密集、IO、JSON、numpy）
4. **热力图**: 框架 × 场景，颜色=req/s

## 复用方式

```bash
# 跑全部测试，生成报告
python benchmarks/suite/runner.py

# 只跑某个框架
python benchmarks/suite/runner.py --framework pyre_subinterp

# 只跑某组场景
python benchmarks/suite/runner.py --group cpu

# 对比两次运行
python benchmarks/suite/runner.py --compare results/2026-03-24_120000 results/2026-03-25_120000
```

## 实施顺序

1. 创建所有 server 脚本（每个框架一个文件，统一路由）
2. 生成 JSON payload 文件
3. 写 `runner.py` 主控 + wrk 结果解析
4. 写 `charts.py` 图表生成
5. 写 HTML 报告模板
6. 跑第一轮全面测试
