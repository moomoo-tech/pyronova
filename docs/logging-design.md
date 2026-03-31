# Pyre 日志系统设计

## 概述

Pyre 的日志系统基于两个原则：
1. **所有 I/O 下沉到 Rust** — Python 永远不直接接触 stdout/stderr 做日志输出
2. **关闭时零开销** — `tracing` 宏编译后仅做一次原子级别检查；被过滤时，不做字符串格式化、不做系统调用、零开销

---

## 架构

```
                    ┌─────────────────────────────────────┐
                    │         Rust tracing-subscriber      │
                    │   (EnvFilter + fmt::Layer)           │
                    │   targets: pyre::server              │
                    │            pyre::access              │
                    │            pyre::app                 │
                    └──────┬─────────┬──────────┬──────────┘
                           │         │          │
              ┌────────────┘         │          └────────────┐
              │                      │                       │
     ┌────────▼────────┐   ┌────────▼────────┐    ┌────────▼────────┐
     │  服务器日志      │   │  访问日志       │    │  应用日志       │
     │  pyre::server   │   │  pyre::access   │    │  pyre::app      │
     │                 │   │                 │    │                 │
     │  - 启动         │   │  - method       │    │  - Python       │
     │  - 关闭         │   │  - path         │    │    logging.*    │
     │  - GIL 看门狗   │   │  - status       │    │  - worker_id   │
     │  - WS 错误      │   │  - latency_us   │    │  - logger 名称 │
     │  - 连接错误     │   │  - mode         │    │  - 文件:行号   │
     └─────────────────┘   └─────────────────┘    └─────────────────┘
          纯 Rust              纯 Rust            Python → Rust FFI
```

---

## 三个日志目标

### 1. `pyre::server` — 服务器生命周期

启动、关闭、看门狗告警、连接错误。

```
INFO  pyre::server Pyre started version="1.2.0" mode="hybrid" addr=127.0.0.1:8000
INFO  pyre::server Shutting down gracefully...
WARN  pyre::server GIL watchdog: main GIL congested latency_ms=52
WARN  pyre::server Connection error error="connection reset by peer"
ERROR pyre::server WebSocket upgrade error error="..."
```

### 2. `pyre::access` — 请求访问日志

每个 HTTP 请求的方法、路径、状态码、延迟和执行模式。

```
INFO  pyre::access Request handled method=GET path=/ status=200 latency_us=198 mode="subinterp"
INFO  pyre::access Request handled method=POST path=/api/users status=201 latency_us=1542 mode="gil"
WARN  pyre::access Client error method=GET path=/missing status=404 latency_us=12
ERROR pyre::access Request failed method=POST path=/crash status=500 latency_us=892
```

### 3. `pyre::app` — Python 应用日志

用户代码 `logging.info()` 通过 FFI 桥接从 Python 路由到 Rust。

```
INFO  pyre::app Fetching users from DB worker=3 logger=myapp file=app.py line=42
ERROR pyre::app Database connection failed worker=7 logger=db file=models.py line=88
```

---

## 配置 API

```python
from pyreframework import Pyre

# 1. 调试模式 — 全量输出，人类可读的文本格式
app = Pyre(debug=True)

# 2. 生产模式 — 仅错误，JSON 格式（适配 ELK/Datadog）
app = Pyre()  # 默认: level=ERROR, access_log=False, format=json

# 3. 自定义 — 精细控制
app = Pyre(log_config={
    "level": "INFO",        # OFF, ERROR, WARN, INFO, DEBUG, TRACE
    "access_log": True,     # 开启每请求日志
    "format": "json",       # json | text
})

# 4. 静默模式 — 压测场景绝对零开销
app = Pyre(log_config={"level": "OFF"})

# 5. enable_logging() — 激活访问日志 + Python 钩子输出
app = Pyre()
app.enable_logging()       # 将级别提升到 INFO，开启 access_log
```

### 环境变量

| 变量 | 效果 |
|---|---|
| `PYRE_LOG=1` | 自动开启日志（等同于 `app.enable_logging()`） |
| `PYRE_METRICS=1` | 开启 GIL 看门狗（10ms 探测间隔） |

---

## Python 日志桥接

### 问题

Python 默认的 `logging.StreamHandler` 在持有 GIL 时同步 `write()` 到 stderr。在 220k QPS 下，这会摧毁吞吐量。

### 解决方案

Pyre 在主解释器和每个子解释器中劫持 Python 的 root logger：

**主解释器** (`app.py`)：
```python
class PyreRustHandler(logging.Handler):
    def emit(self, record):
        emit_python_log(           # PyO3 FFI → Rust
            level=record.levelname,
            name=record.name,
            message=record.getMessage(),
            pathname=record.pathname,
            lineno=record.lineno,
        )
```

**子解释器** (`_bootstrap.py`)：
```python
class _PyreRustHandler(logging.Handler):
    def emit(self, record):
        _pyre_emit_log(            # C-FFI → Rust（与 pyre_recv/pyre_send 同类注册方式）
            record.levelname,
            record.name,
            record.getMessage(),
            record.pathname or "",
            record.lineno or 0,
            self._worker_id,
        )
```

### 性能特征

| 场景 | 开销 |
|---|---|
| `level=OFF` | ~1ns（原子比较，分支预测跳过） |
| `level=INFO`，日志被过滤 | ~1ns（同上） |
| `level=INFO`，日志被接受 | ~50-100ns FFI 穿越 + tracing 格式化 |
| Python `logger.info("msg")` | ~200ns（getMessage + FFI） |
| Python `logger.info("data: %s", huge_dict)` | `%s` 格式化的开销（不可避免） |

---

## Rust 实现

### 修改的文件

| 文件 | 变更 |
|---|---|
| `Cargo.toml` | 添加 `tracing`、`tracing-subscriber`（env-filter, json） |
| `src/logging.rs` | **新增** — `init_logger()`、`emit_python_log()` PyO3 函数 |
| `src/lib.rs` | 注册 `logging` 模块 + 函数 |
| `src/app.rs` | 启动/关闭 → `tracing::info!`，连接错误 → `tracing::warn!` |
| `src/handlers.rs` | 访问日志：`latency_us`、`method`、`path`、`status`、`mode` |
| `src/interp.rs` | `pyre_emit_log_cfunc` C-FFI，注册到所有子解释器 |
| `src/monitor.rs` | GIL 看门狗 → `tracing::warn!` |
| `src/websocket.rs` | WebSocket 错误 → `tracing::error!`/`tracing::warn!` |

### 关键设计决策

1. **`EnvFilter` 实现零开销关闭** — 当级别为 OFF 或被过滤时，`tracing::info!` 编译为单次原子加载 + 分支跳转。CPU 分支预测器预热后命中率 100%。

2. **独立的 `pyre::access` 目标** — 允许用户关闭访问日志但保留服务器/应用日志，反之亦然。通过 `access_log` 配置映射为 `pyre::access=off` 指令。

3. **子解释器的 C-FFI 桥接** — 子解释器无法导入 PyO3 扩展模块。`_pyre_emit_log` 作为 C-FFI 内建函数注册（类似 `pyre_recv`/`pyre_send`），在引导脚本运行前注入到 globals。

4. **`init_logger` 延迟到 `run()`** — 允许 `enable_logging()` 在 tracing subscriber 锁定前修改日志配置。`tracing-subscriber` 每个进程只允许初始化一次。

5. **启动横幅保留 `println!`** — 人类可读的启动横幅（`Pyre v1.2.0 [hybrid mode]...`）与 `tracing::info!` 并存，因为它是始终可见的开发者体验，不是可过滤的日志输出。

---

## 测试

`tests/test_logging.py` 覆盖 8 个场景：

| 测试 | 验证内容 |
|---|---|
| `test_gil_mode_logging` | GIL 模式下 Python 钩子输出（`[INFO ] GET / → 200`） |
| `test_subinterp_rust_logging` | 子解释器模式下 Rust tracing 访问日志 |
| `test_user_print_in_subinterp` | 子解释器中 `print()` 正常工作 |
| `test_user_logging_in_subinterp` | Python `logging.info()` 桥接到 Rust tracing |
| `test_debug_mode_tracing` | `debug=True` 产生服务器生命周期 tracing 输出 |
| `test_debug_mode_access_log` | `debug=True` 产生带延迟的访问日志 |
| `test_python_logging_bridge_main` | 主解释器日志桥接工作正常 |
| `test_json_format` | JSON 格式输出包含结构化字段 |
