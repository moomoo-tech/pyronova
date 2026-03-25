# 开发者体验 (Developer Experience) 指南

## 模式选择

```
Quick guide:
  All def handlers      → app.run()              最快, 220k req/s
  Any async def + await → app.run(mode="async")   I/O密集, 133k req/s
  Need numpy/C扩展      → @app.get(gil=True)      Hybrid 混合调度
```

### 详细决策矩阵

| 你的场景 | 推荐模式 | 为什么 |
|---------|---------|--------|
| REST API / JSON 路由 | `app.run()` | 默认 subinterp，极致吞吐 |
| 数据库查询 (asyncpg) | `mode="async"` | await 不阻塞 worker |
| 调用外部 API (httpx) | `mode="async"` | 并发 I/O |
| asyncio.sleep / 定时器 | `mode="async"` | 协程挂起不占线程 |
| numpy / pandas 计算 | `gil=True` 路由 | C 扩展需要主解释器 |
| 混合：部分 numpy + 部分 API | Hybrid | 快路由走 subinterp，numpy 走 GIL |
| AI Agent (LLM 调用) | `mode="async"` | await LLM 响应 |
| SSE 流式输出 | `gil=True` + SkyStream | 长连接流式传输 |

### 混用 sync + async handler

`mode="async"` 同时支持 `def` 和 `async def`：

```python
@app.get("/fast")
def fast(req):              # sync — 直接执行
    return "hello"

@app.get("/io")
async def io_heavy(req):    # async — await 不阻塞
    await asyncio.sleep(0.1)
    return "done"

app.run(mode="async")       # 两种都能跑
```

注意：sync handler 在 async 模式下有 ~35% 吞吐量开销（asyncio 引擎中转）。
如果全是 sync handler，用默认模式（`app.run()`）更快。

### 不需要选择的情况

- 如果所有 handler 都是普通 `def` 且没有 I/O 等待 → 直接 `app.run()`，不传任何参数
- 如果混用 sync + async → `app.run(mode="async")`
- 框架默认使用 `subinterp` 模式，这是最快的

### 子解释器内存效率

每个子解释器增量 ~10 MB（共享进程代码段和共享库）：

| Workers | Pyre SubInterp | Robyn 多进程 |
|---------|---------------|-------------|
| 1 | 31 MB | ~20 MB |
| 10 | 119 MB | 447 MB |
| 32 | 333 MB | ~660 MB |

共享的：CPython 核心代码、Rust 二进制、`.so` 共享库的只读代码段。
不共享的：Python 堆（模块字典、用户对象）、GIL 状态。
C 扩展（numpy/orjson）的 `.so` 文件只 mmap 一次，32 workers 摊薄基线。

## 日志

### 启用框架日志

```python
app = Pyre()
app.enable_logging()  # 开启结构化日志
```

输出格式：
```
GIL 模式:      2026-03-24 17:30:01 [INFO ] GET /api/trade → 200 (2.3ms)
Sub-interp:    [INFO ] GET /api/trade → 200
```

### 用户自定义日志

在 handler 里直接用 Python 标准 `logging` 或 `print()`，两种模式下都正常工作：

```python
import logging
logger = logging.getLogger("myapp")

@app.get("/trade")
def trade(req):
    logger.info(f"Processing trade: {req.path}")
    print("debug info", flush=True)  # flush=True 保证立即可见
    return {"status": "ok"}
```

### 日志级别

```python
app.enable_logging(level="info")    # INFO/WARN/ERROR (默认)
app.enable_logging(level="error")   # 只显示错误
app.enable_logging(level="debug")   # 全部显示
```

## Pydantic 集成

自动解析和校验请求体：

```python
from pydantic import BaseModel, Field

class TradeOrder(BaseModel):
    ticker: str = Field(max_length=5)
    amount: int = Field(gt=0)
    price: float

@app.post("/trade", model=TradeOrder)
def trade(req, order: TradeOrder):
    # order 已经校验通过，类型安全
    return {"ticker": order.ticker, "total": order.amount * order.price}
```

无效请求自动返回 422：
```json
{"error": "ValidationError: 1 validation error for TradeOrder\namount\n  Input should be greater than 0"}
```

## RPC 调用

### 服务端

```python
@app.rpc("/rpc/add")
def add(data):
    return {"sum": data["a"] + data["b"]}
```

自动支持 MsgPack 和 JSON，根据 `Content-Type` 头切换。

### 客户端

```python
from skytrade import PyreRPCClient

with PyreRPCClient("http://127.0.0.1:8000") as client:
    result = client.add(a=3, b=5)  # 像本地函数一样调用
    print(result)  # {"sum": 8}
```

## MCP (AI Agent 工具集成)

```python
@app.mcp.tool(description="Add two numbers")
def add(a: int, b: int) -> int:
    return a + b

@app.mcp.resource("config://app")
def get_config():
    return {"version": "1.0.0"}
```

Claude Desktop 连接 `http://localhost:8000/mcp` 即可发现工具。

## WebSocket

```python
@app.websocket("/ws")
def echo(ws):
    while True:
        msg = ws.recv()       # 文本
        if msg is None: break
        ws.send(f"echo: {msg}")

# 二进制
data = ws.recv_bytes()
ws.send_bytes(data)

# 混合
msg_type, data = ws.recv_message()  # ("text", "hello") or ("binary", b"\x00")
```

## SSE 流式传输

```python
import threading
from skytrade import SkyStream

@app.get("/stream", gil=True)
def stream(req):
    s = SkyStream()
    def generate():
        for token in ["Hello", " ", "World"]:
            s.send_event(token)
        s.close()
    threading.Thread(target=generate).start()
    return s
```

## SharedState (跨 worker 共享)

```python
# 写入（任意 worker）
app.state["session:user_1"] = '{"role": "admin"}'

# 读取（任意 worker）
data = app.state["session:user_1"]

# 字典操作
len(app.state)
"key" in app.state
del app.state["key"]
```

纳秒级延迟，零 Redis 依赖。`gil=True` 路由中使用。

## GIL 监控

```python
# 启用 (环境变量)
# PYRE_METRICS=1 python app.py

from skytrade import get_gil_metrics
last, peak, probes, total, rss, queue, hold, dropped, total_req = get_gil_metrics()
```

## IDE 支持

`engine.pyi` 提供完整的类型提示。IDE 中 `req.` 会自动补全：
- `req.method`, `req.path`, `req.params`, `req.query`, `req.headers`
- `req.body`, `req.text()`, `req.json()`, `req.query_params`

## 内存效率

每个子解释器增量 ~10 MB（共享进程代码段）：

```
Workers=1   31 MB
Workers=10  119 MB
Workers=32  333 MB
```

对比 Robyn 多进程：22 进程 = 427 MB。Pyre 节省 ~50-70%。
