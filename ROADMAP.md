# Pyre Roadmap

## Phase 1 — Skeleton (DONE)
- Tokio + Hyper HTTP server
- PyO3 0.28 bridge
- matchit routing
- GIL detach/attach
- Benchmark: 69k req/s vs Robyn 27k (2.5x), 10MB vs 35MB RSS

## Phase 2 — Optimization (DONE)
- Multi-worker Tokio runtime (configurable)
- Keep-alive + pipeline flush
- Graceful shutdown (Ctrl+C)
- Rust-side JSON serialization (serde_json, skip Python json.dumps)
- dict/list 直接返回支持
- Connection error 静噪

## Phase 3 — Surpass Robyn
- [ ] io_uring backend (Linux, monoio)
- [ ] HTTP/3 QUIC
- [ ] Native WebSocket
- [ ] Native gRPC (tonic crate, Robyn 不支持)

## Phase 4 — True Parallelism (核心突破)

目标：支持两种并行模式，用户可选：

### 模式 A: Free-threaded Python (python3.14t, no-GIL)
- PyO3 0.28 已支持
- 改动最小，装 python3.14t 即可
- 所有 Tokio worker 线程共享同一解释器，真并行调用 handler
- 优先实现

### 模式 B: Per-Interpreter GIL (子解释器)
- 每个 worker 线程绑定一个独立子解释器，各自有独立 GIL
- 提供进程级隔离 + 线程级性能
- 架构：Tokio Worker N → Sub-Interpreter N (GIL-N) → handler()

#### 子解释器的核心难题：PyO3 不支持

PyO3 0.28 明确阻止子解释器（`#[pymodule]` 会检查 interpreter ID，第二次 import 直接 ImportError）。
根本原因：PyO3 内部大量全局 `static` 状态，跨解释器共享会 unsound。

Tracking issues:
- https://github.com/PyO3/pyo3/issues/576
- https://github.com/PyO3/pyo3/issues/3451
- https://github.com/PyO3/pyo3/issues/4570

#### 方案：Fork PyO3，手撸多解释器版本

核心改造：
1. **消除全局 static** — 将所有模块级全局状态（PyOnceLock、static module def、type object cache）
   迁移到 `PyModule_GetState` 或 `PyInterpreterState_GetDict`，实现 per-interpreter 隔离
2. **多阶段初始化** — 确保 `#[pymodule]` 生成的模块使用 PEP 489 multi-phase init，
   并声明 `Py_mod_multiple_interpreters = Py_MOD_PER_INTERPRETER_GIL_SUPPORTED`
3. **移除 interpreter ID 检查** — 去掉 `make_module` 中的 AtomicI64 拦截逻辑
4. **ThreadState 管理** — 在 Tokio worker 线程中正确切换 `PyThreadState_Swap`
5. **FFI 层** — 使用 `Py_NewInterpreterFromConfig` + `PyInterpreterConfig_OWN_GIL`

风险评估：
- 工作量大，PyO3 内部状态分布广
- 需要持续跟进上游 PyO3 变更
- 但如果成功，这将是**全球第一个**支持 per-interpreter GIL 的 Rust-Python web 框架
- 目前没有任何生产级框架（包括 Granian）实现了这个

#### 里程碑
1. Fork PyO3 → 审计所有全局 static 变量
2. PoC: 在两个子解释器中成功 import 同一个 `#[pymodule]`
3. 集成到 Pyre: worker pool + interpreter pool
4. Benchmark: 对比 free-threaded / sub-interpreter / Robyn --fast

## Phase 5 — 生产级子解释器（稳定性）

当前子解释器实现是 raw FFI PoC，存在以下隐患：

| 问题 | 风险等级 | 说明 |
|------|---------|------|
| 裸 `*mut ffi::PyObject` 指针 | 高 | 一个 DECREF 错了就 segfault 或内存泄漏 |
| 手动 `Py_INCREF/DECREF` | 高 | 无 RAII 保护，容易漏 |
| 脚本过滤是字符串匹配 | 中 | `app.` 开头的用户变量会被误删 |
| `_SkyRequest` 是纯 Python 重写 | 中 | 跟主解释器的 `SkyRequest` 行为可能不一致 |
| 无 `Py_EndInterpreter` 清理 | 中 | 进程退出时子解释器未正确 shutdown |
| 子解释器里不能用 PyO3 扩展 | **致命** | 用户 handler 里 `import numpy` 直接炸 |

### Step 1: 安全抽象层 `pyre-interp`（短期，不需要 fork PyO3）

在 `pyo3::ffi` 之上包一层：
- [ ] RAII 包装 `PyObject` 引用计数（Drop 自动 DECREF）
- [ ] 子解释器 `Drop` 时自动 `Py_EndInterpreter`
- [ ] 用 Python AST 解析替代字符串行过滤
- [ ] `_SkyRequest` protocol 兼容测试
- [ ] per-worker Mutex 改为 channel-based worker pool（避免锁竞争）

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

## 长期愿景
- 成为第一个同时支持 free-threaded 和 per-interpreter GIL 的 Python web 框架
- 在交易系统场景中实现微秒级 WebSocket + Orderbook 处理
- 提供 Robyn/Flask/FastAPI 的迁移兼容层
