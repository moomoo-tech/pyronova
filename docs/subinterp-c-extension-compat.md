# Sub-interpreter C/C++ Extension 兼容性技术路线

> 核心问题：子解释器模式下 `import numpy` / `import orjson` 等 legacy C/C++ 扩展会 ImportError。

## 背景

CPython 3.12+ 的子解释器引入了 `PyInterpreterConfig` 配置项：

```c
typedef struct {
    int check_multi_interp_extensions;  // 关键 flag
    int gil;                            // OWN_GIL = 独立 GIL
    // ...
} PyInterpreterConfig;
```

当 `check_multi_interp_extensions = 1`（严格模式）时，CPython 会在 `import` 时检查 C 扩展模块是否声明了：

```c
Py_mod_multiple_interpreters = Py_MOD_PER_INTERPRETER_GIL_SUPPORTED
```

**绝大多数 legacy C/C++ 扩展都没有声明这个 flag**，包括：
- numpy（2.x 在逐步适配，但截至 2026-03 仍未完全声明）
- orjson
- pydantic-core
- pandas（底层依赖 numpy）
- scipy
- pillow
- 所有基于 PyO3 的扩展（PyO3 不支持多解释器）

## 为什么 legacy C/C++ 扩展有风险

C 扩展在模块级使用全局 `static` 变量是常见模式：

```c
// numpy 内部类似的模式
static PyObject *numpy_int_type = NULL;  // 全局共享

PyMODINIT_FUNC PyInit__multiarray_umath(void) {
    numpy_int_type = create_int_type();  // 初始化一次
    // ...
}
```

在单解释器下这没问题。但在多个子解释器共享同一进程时：
1. **数据竞争**：两个子解释器的线程同时读写全局 `static`
2. **类型混淆**：子解释器 A 创建的 PyObject 被子解释器 B 引用（不同 GIL 域）
3. **引用计数错乱**：DECREF 在错误的解释器上下文中执行

但实际上，**大部分知名库已经在内部做了足够的隔离**，只是还没有正式声明 flag。

## 技术路线

### Phase A：宽松模式（当前实施）

```rust
let config = ffi::PyInterpreterConfig {
    check_multi_interp_extensions: 0,  // 允许未声明的扩展加载
    gil: ffi::PyInterpreterConfig_OWN_GIL,
    // ...
};
```

**优点**：
- 一行改动，立即生效
- numpy、orjson 等大概率能正常工作
- 用户可以在 handler 里自由 import

**风险**：
- 扩展如果有真正不安全的全局状态，可能 segfault
- 没有编译期或加载期保护，问题在运行时才暴露
- 不同子解释器间的类型对象不隔离

**缓解措施**：
- 添加运行时 import 失败的 warning 日志
- 文档注明已测试通过的库清单
- 提供 `strict_extensions=True` 选项让用户回退严格模式

### Phase B：Fork PyO3（中期，解锁 Pyre 自身模块）

改 PyO3 两处，让 Pyre 的 `#[pymodule]` 能在子解释器中加载：

1. `pymodule.rs` — 去掉 `make_module` 的 `AtomicI64` interpreter ID 检查
2. `#[pymodule]` 的全局 `static` 状态 → 迁移到 `PyModule_GetState`（per-interpreter 隔离）

**效果**：
- `SkyRequest` / `SkyResponse` 的 `#[pyclass]` 直接在子解释器中可用
- 干掉 `_SkyRequest` / `_SkyResponse` 纯 Python 替身（减少维护 + 统一行为）
- 基于 PyO3 的第三方扩展也能通过 fork 版本编译使用

**不解决的问题**：
- 非 PyO3 的 legacy C 扩展（numpy 等）仍需 Phase A 的宽松模式

### Phase C：等待上游生态适配（长期）

跟踪以下上游进展：

| 库 | Tracking Issue | 状态 (2026-03) |
|----|---------------|----------------|
| PyO3 | [#3451](https://github.com/PyO3/pyo3/issues/3451) | 讨论中，无 ETA |
| numpy | [#24003](https://github.com/numpy/numpy/issues/24003) | 2.x 逐步适配中 |
| orjson | — | 未开始 |
| pydantic-core | — | 依赖 PyO3 |
| CPython | [PEP 734](https://peps.python.org/pep-0734/) | 3.14 引入 `interpreters` stdlib |

当上游库声明 `Py_MOD_PER_INTERPRETER_GIL_SUPPORTED` 后，可以将 `check_multi_interp_extensions` 切回 1（严格模式），获得完整的安全保证。

## 已测试的兼容性矩阵

| 库 | 版本 | 宽松模式 | 严格模式 | 备注 |
|----|------|---------|---------|------|
| numpy | 2.4.3 | ⚠️ 单 worker 可用 | ❌ ImportError | 第二个子解释器 `import numpy` → `ImportError: cannot load module more than once per process`；并发压测 → segfault |
| orjson | 3.11.7 | ✅ **完全正常** | ❌ ImportError | 多子解释器并发无问题 |
| json (stdlib) | — | ✅ | ✅ | 纯 Python |
| urllib (stdlib) | — | ✅ | ✅ | 纯 Python |

> 测试日期：2026-03-24, Python 3.14.3, macOS ARM64

### numpy 的具体问题

numpy 的 `_multiarray_umath` C 扩展模块在模块初始化时检查自身是否已被加载：
```
ImportError: cannot load module more than once per process
```
这是 numpy 主动拒绝在第二个子解释器中加载，不是 CPython 的限制。
即使第一个子解释器能成功 `import numpy`，后续子解释器都会失败。
并发场景下（多个子解释器同时使用 numpy 的全局状态）会 segfault。

**结论**：numpy 在子解释器模式下不可用。需要使用 GIL 模式，或者等待 numpy 官方支持。

## 配置方式

```python
# 默认：宽松模式（允许 legacy C 扩展）
app.run(mode="subinterp")

# 严格模式（仅允许声明了多解释器支持的扩展）
app.run(mode="subinterp", strict_extensions=True)
```
