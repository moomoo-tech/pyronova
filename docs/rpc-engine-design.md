# Sky-RPC 引擎设计文档

> 状态：愿景阶段，Web 框架 (Pyre) PyPI 发布后启动

## 定位

Pyre 的第二阶段产品：从 Web 框架扩展为高性能 RPC 引擎。

```
阶段 1 (当前): Pyre Web Framework — HTTP/WS/SSE/MCP, 碾压 Robyn ✅
阶段 2 (未来): Sky-RPC Engine — 二进制协议, Proto, 微服务内部通信
```

## 核心设计：Rust 主导的 Protobuf 管道

```
网卡 DMA → io_uring/epoll → Rust Framer (16B header)
    → prost 反序列化 (Rust, 零 Python)
    → PyO3 #[pyclass] 代理视图
    → Python handler (仅业务逻辑)
    → 响应序列化 (Rust)
    → 回写 TCP
```

## 二进制帧协议 (16 字节固长 Header)

| 字段 | 字节 | 类型 | 说明 |
|------|------|------|------|
| Magic | 2 | u16 | 0x534B ("SK") 快速丢弃非法连接 |
| Flags | 1 | u8 | 压缩/心跳/单向标志 |
| Method ID | 1 | u8 | 路由 ID (0-255, O(1) 数组索引) |
| Request ID | 4 | u32 | 多路复用请求匹配 |
| Trace ID | 4 | u32 | 链路追踪 |
| Length | 4 | u32 | Payload 字节数 (最大 4GB) |

## O(1) 路由表

```rust
struct RpcDispatcher {
    routes: [Option<PyObject>; 256],  // 数组索引 = 极致 O(1)
}

fn dispatch(&self, method_id: u8, payload: &[u8]) {
    self.routes[method_id as usize]  // 无哈希、无字符串比较
}
```

## 代码生成器 (skyrpc-gen)

输入: `trade.proto`
输出:
1. `generated.rs` — prost 结构体 + PyO3 wrapper
2. `trade_pb2.pyi` — Python type stubs (IDE 补全)
3. Method ID 映射表

```python
# 用户体验
class MyTradeService(TradeServiceBase):
    def place_order(self, request: OrderRequest) -> dict:
        return {"status": "ok", "symbol": request.symbol}
```

## 与 Pyre Web 框架的关系

```
┌─────────────────────────────────────┐
│         用户的 Python 代码           │
├──────────────┬──────────────────────┤
│  Pyre Web    │    Sky-RPC Engine    │
│  HTTP/WS/SSE │    Binary Proto     │
│  MCP/REST    │    gRPC compat      │
├──────────────┴──────────────────────┤
│     共享层：子解释器池 + SharedState  │
│     + GIL Watchdog + Arena Pool     │
├─────────────────────────────────────┤
│     Tokio / Monoio (可插拔)         │
└─────────────────────────────────────┘
```

## 实施计划

| 阶段 | 内容 | 前置条件 |
|------|------|---------|
| 0 | Pyre Web 发布 PyPI | ← **当前优先** |
| 1 | 16B framer + prost 集成 | Pyre 稳定 |
| 2 | skyrpc-gen 代码生成器 | Proto 解析器 |
| 3 | O(1) 路由 + 子解释器调度 | 复用 Pyre interp.rs |
| 4 | gRPC 兼容层 (HTTP/2 + Proto) | HTTP/2 已有 |
| 5 | Monoio thread-per-core 引擎 | Linux 机器 |
