# Pyre Roadmap

## Phase 1 — Skeleton (DONE ✓) — 2026-03-23

- Tokio + Hyper HTTP server
- PyO3 0.28 bridge
- matchit routing
- GIL detach/attach
- Benchmark: 69k req/s vs Robyn 27k (2.5x), 10MB vs 35MB RSS

## Phase 2 — Optimization (DONE ✓) — 2026-03-23

- Multi-worker Tokio runtime (configurable)
- Keep-alive + pipeline flush
- Graceful shutdown (Ctrl+C)
- Rust-side JSON serialization (serde_json, skip Python json.dumps)
- dict/list 直接返回支持
- Connection error 静噪
- Benchmark: GIL 100k, SubInterp 216k, Robyn 76k

## Phase 3 — Developer Experience (DONE ✓) — v0.3.0, 2026-03-24

- [x] `Pyre` 装饰器语法 (`@app.get("/")`)
- [x] `SkyResponse` — 自定义 status code / headers / content-type
- [x] `req.headers` — 请求头读取
- [x] `req.query_params` — 查询参数解析
- [x] `before_request` / `after_request` 中间件
- [x] `@app.fallback` — 自定义 404 handler
- [x] 静态文件服务 (`app.static("/prefix", "./dir")`)
- [x] 内置请求日志 (`app.enable_logging()`)
- [x] PATCH / OPTIONS / HEAD 方法支持
- [x] PyPI 就绪 (pyproject.toml + classifiers)
- Benchmark: 213k req/s SubInterp, 100k GIL (零性能回归)

## Phase 4 — True Parallelism (DONE ✓) — 2026-03-23

目标：支持两种并行模式，用户可选。

### 模式 A: Free-threaded Python (python3.14t, no-GIL)
- PyO3 0.28 已支持
- 改动最小，装 python3.14t 即可
- 所有 Tokio worker 线程共享同一解释器，真并行调用 handler
- 优先实现

### 模式 B: Per-Interpreter GIL (子解释器)
- 每个 worker 线程绑定一个独立子解释器，各自有独立 GIL
- 提供进程级隔离 + 线程级性能
- 架构：Tokio Worker N → Sub-Interpreter N (GIL-N) → handler()
- Benchmark: 达到纯 Rust 97% 性能

#### 子解释器的核心难题：PyO3 不支持

PyO3 0.28 明确阻止子解释器（`#[pymodule]` 会检查 interpreter ID，第二次 import 直接 ImportError）。
根本原因：PyO3 内部大量全局 `static` 状态，跨解释器共享会 unsound。

Tracking issues:
- https://github.com/PyO3/pyo3/issues/576
- https://github.com/PyO3/pyo3/issues/3451
- https://github.com/PyO3/pyo3/issues/4570

#### 解法：绕过 PyO3，直接 FFI

不 fork PyO3，而是在子解释器中用 raw `pyo3::ffi` 调用 CPython C API：
- `Py_NewInterpreterFromConfig` + `PyInterpreterConfig_OWN_GIL` 创建独立 GIL 子解释器
- `PyRun_String` 执行用户脚本（过滤掉框架代码）
- `PyDict_GetItemString` 提取 handler 函数指针
- `PyObject_Call` 直接调用 handler
- 纯 Python `_SkyRequest` / `_SkyResponse` 替代 PyO3 的 `#[pyclass]`

风险：裸指针、手动引用计数、无 RAII，但性能极佳。

#### 里程碑（已完成）
1. ~~Fork PyO3~~ → 改为 raw FFI 方案
2. PoC: 10 个子解释器并行处理请求 ✓
3. 集成到 Pyre: worker pool + interpreter pool ✓
4. Benchmark: SubInterp 216k vs Robyn 76k (2.8x) ✓

## Phase 5 — 生产级子解释器 (DONE ✓ Step 1) — v0.3.1, 2026-03-24

原始问题清单（Phase 4 遗留的 raw FFI PoC 隐患）：

| 问题 | 风险等级 | 状态 | 说明 |
|------|---------|------|------|
| 裸 `*mut ffi::PyObject` 指针 | 高 | ✅ 已修复 | `PyObjRef` RAII 包装，Drop 自动 DECREF |
| 手动 `Py_INCREF/DECREF` | 高 | ✅ 已修复 | `from_owned` / `from_borrowed` / `into_raw` 安全接口 |
| 脚本过滤是字符串匹配 | 中 | ✅ 已修复 | 改用 Python `ast.parse` + `ast.unparse`，精准过滤 |
| `_SkyRequest` 是纯 Python 重写 | 中 | 🔄 保留 | 跟主解释器行为一致，暂无问题 |
| 无 `Py_EndInterpreter` 清理 | 中 | ✅ 已修复 | worker 线程退出时自动 `Py_EndInterpreter` |
| 子解释器里不能用 PyO3 扩展 | **致命** | 🔄 Step 2 | 需要 fork PyO3，见下方 |
| 队头阻塞 (Round-robin + Mutex) | 高 | ✅ 已修复 | 改为 `crossbeam-channel` 多消费者池，真负载均衡 |
| 静态文件目录穿越漏洞 | 高 | ✅ 已修复 | `trim_start_matches('/')` + `..` 检测 |
| Middleware 破坏二进制响应 | 中 | ✅ 已修复 | 非 UTF-8 body 使用 `PyBytes` 而非强转空字符串 |

### Step 1: 安全抽象层 `pyre-interp` (DONE ✓)

在 `pyo3::ffi` 之上包一层（`src/interp.rs`, 820 行）：
- [x] `PyObjRef` RAII 包装引用计数（Drop 自动 DECREF）
- [x] `from_owned` / `from_borrowed` / `into_raw` 安全接口
- [x] `py_str` / `py_bytes` / `py_str_dict` 辅助构造器
- [x] 子解释器退出时自动 `Py_EndInterpreter`
- [x] Python AST 解析替代字符串行过滤（支持装饰器语法）
- [x] `crossbeam-channel` 多消费者 worker pool（消除队头阻塞）
- [x] `tokio::sync::oneshot` 异步响应（Tokio 任务不阻塞）

### 工程重构：模块化拆分 (DONE ✓)

从 981 行 God File 拆分为 8 个职责单一的模块：

```
src/
├── lib.rs          18 行   ← #[pymodule] + mod 声明
├── types.rs       116 行   ← SkyRequest, SkyResponse, extract_headers
├── json.rs         44 行   ← py_to_json_value (Rust-side JSON 序列化)
├── router.rs       70 行   ← RouteTable, SharedRoutes
├── response.rs    164 行   ← extract_response_data, build/error/404 response
├── handlers.rs    218 行   ← handle_request (GIL), handle_request_subinterp
├── static_fs.rs    69 行   ← try_static_file, mime_from_ext
├── app.rs         342 行   ← SkyApp, run_gil(), run_subinterp()
└── interp.rs      820 行   ← PyObjRef RAII, channel pool, AST filter
```

Benchmark: SubInterp 215k (channel pool, -0.5% vs mutex), GIL 104k (+3%), Robyn 83k。零回归。

### Step 2: 精准 Fork PyO3（中期，解锁第三方扩展）

**不需要重写整个 PyO3**，只改两处：
1. `pymodule.rs` — 去掉 `make_module` 中的 `AtomicI64` interpreter ID 检查
2. `#[pymodule]` 的 `static` 状态 — 迁移到 `PyModule_GetState`（per-interpreter 隔离）

改完后：
- Pyre 自己的 `#[pymodule]` 能在子解释器中加载
- `SkyRequest` 等 `#[pyclass]` 可以直接传入子解释器，不再需要 `_SkyRequest` 纯 Python 替身
- 声明了 `Py_MOD_PER_INTERPRETER_GIL_SUPPORTED` 的第三方 C 扩展也能用（numpy、orjson 等正在逐步加这个声明）

### Step 3: 等待/贡献上游（长期）

- PyO3 tracking issue: https://github.com/PyO3/pyo3/issues/3451
- 如果 Step 2 的 fork 稳定，可以向 PyO3 上游提 PR
- 关注 numpy/orjson/pydantic 等库的 `Py_MOD_PER_INTERPRETER_GIL_SUPPORTED` 适配进度

## Phase 6 — 协议突破 (进行中) — v0.4.0

### WebSocket (DONE ✓) — 2026-03-24

原生 WebSocket 支持，基于 `tokio-tungstenite`。

**架构**：
```
Client ←WebSocket→ Tokio (async) ←channels→ Python handler thread (sync)
                    ↑                         ↑
            tokio-tungstenite          ws.recv() / ws.send()
            async read/write           std::sync::mpsc (incoming)
                                       tokio::sync::mpsc (outgoing)
```

- 每个 WebSocket 连接分配一个 OS 线程运行 Python handler
- `SkyWebSocket` pyclass 提供 `recv()` (阻塞) / `send(msg)` / `close()` 接口
- 自动处理 Ping/Pong、连接关闭
- 与 HTTP 路由共存，同一端口同时服务 HTTP + WebSocket
- GIL 模式和 Hybrid 模式均支持

**API**：
```python
@app.websocket("/ws")
def echo(ws):
    while True:
        msg = ws.recv()    # 阻塞等待
        if msg is None:
            break          # 客户端断开
        ws.send(f"echo: {msg}")
```

### 待完成
- [ ] HTTP/2（deps 里已有 hyper http2 feature）
- [ ] io_uring backend (Linux, monoio)
- [ ] HTTP/3 QUIC
- [ ] Native gRPC (tonic crate, Robyn 不支持)
- [ ] WebSocket 二进制消息支持
- [ ] WebSocket 子解释器模式（当前走 GIL）

## 长期愿景
- 成为第一个同时支持 free-threaded 和 per-interpreter GIL 的 Python web 框架
- 在交易系统场景中实现微秒级 WebSocket + Orderbook 处理
- 提供 Robyn/Flask/FastAPI 的迁移兼容层

---

## 性能历史

| 日期 | 版本 | SubInterp | GIL | Robyn | 里程碑 |
|------|------|-----------|-----|-------|--------|
| 2026-03-23 | v0.1.0 | — | 69k | 27k | Phase 1: 骨架，2.5x Robyn |
| 2026-03-23 | v0.2.0 | 216k | 100k | 76k | Phase 2+4: 子解释器，2.8x Robyn |
| 2026-03-24 | v0.3.0 | 213k | 100k | 76k | Phase 3: DX 功能补全，零回归 |
| 2026-03-24 | v0.3.1 | 215k | 104k | 83k | Phase 5.1: RAII + channel pool + 模块化拆分 + 安全修复 |
| 2026-03-24 | v0.4.0 | 215k | 104k | 83k | Phase 6: Native WebSocket + Hybrid GIL + 全面压测(54场景) |
