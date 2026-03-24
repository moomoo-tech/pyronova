# Zero-Copy 数据传递设计文档

> 适用场景：量化行情推送、AI Agent 大 JSON payload、高频二进制协议

## 核心原则

在多子解释器架构中：
1. **绝不跨解释器共享 PyObject** — segfault
2. **让 Rust 充当中立内存池** — Python 只拿引用
3. **请求级生命周期** — Arena 分配器，请求结束自动回收

## 方法一：C 结构体映射（定长数据）

适合：股票行情 tick、传感器数据、固定格式消息

```rust
#[repr(C)]
pub struct MarketTick {
    pub price: f64,
    pub volume: i32,
    pub symbol_id: i32,
}

// Rust 分配，返回指针地址给 Python
let tick = Box::new(MarketTick { price: 150.25, volume: 1000, symbol_id: 42 });
let ptr = Box::into_raw(tick) as usize;
```

```python
import ctypes

class MarketTick(ctypes.Structure):
    _fields_ = [("price", ctypes.c_double), ("volume", ctypes.c_int), ("symbol_id", ctypes.c_int)]

tick = ctypes.cast(ptr, ctypes.POINTER(MarketTick)).contents
# O(1) 访问，无拷贝
```

## 方法二：Buffer 协议（变长数据）

适合：AI Agent 长文本、RAG 向量、大 JSON

```rust
// Rust 维护 Vec<u8>，通过 PyO3 暴露为 memoryview
fn get_payload_view<'py>(&'py self, py: Python<'py>) -> &'py PyMemoryView {
    PyMemoryView::from_bound(py, &self.raw_payload)
}
```

```python
import orjson

# orjson 直接读 memoryview 底层内存 — 两个 Rust 模块"隔空传功"
parsed = orjson.loads(payload_view)  # 零拷贝解析
```

## Arena 分配器（请求级内存管理）

```rust
use bumpalo::Bump;

struct RpcContext {
    arena: Bump,        // 请求级内存池
    raw_payload: Vec<u8>,
}

// 请求结束 → ctx drop → arena 一次性释放 → O(1) 清理
```

## 实施路线

| 阶段 | 内容 | 何时做 |
|------|------|--------|
| 1 | `req.body` 返回 `memoryview` 而非拷贝 | 当大 payload 成为瓶颈 |
| 2 | `SharedState.get_view()` 返回零拷贝视图 | 量化行情共享池 |
| 3 | Arena allocator per request | 高频交易场景 |
| 4 | `#[repr(C)]` 结构体直传 | 固定格式行情数据 |

## 当前状态

215k QPS 基线下，序列化不是瓶颈。`orjson` 自动检测已在 sub-interpreter 中生效。
这些优化留给真正需要处理 10MB+ payload 或微秒级行情推送的场景。
