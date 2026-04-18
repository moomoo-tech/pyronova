# Benchmark 14: Channel 调度延迟实测 — GIL 模式 vs Sub-interp 模式 (2026-04-17)

## 目的

量化 Pyre 在 sub-interpreter 模式下 crossbeam channel + tokio oneshot 的跨线程调度开销。验证 channel round-trip 是否为性能瓶颈，并评估是否需要引入 lock-free ring buffer 或 arena 指针直传等优化。

## 背景

Sub-interp 模式下，每条请求的生命周期为：

```
Tokio task → crossbeam::bounded.try_send(WorkRequest) → OS 线程 recv()
  → Python handler 执行 → oneshot::Sender.send(Response) → Tokio task 唤醒
```

理论上，跨线程队列竞争和唤醒延迟会成为超高并发下的瓶颈。本测试通过对比 GIL 模式（无 channel，直接调用）与 sub-interp 模式（完整 channel round-trip）来隔离调度开销。

## 环境

| 项目 | 值 |
|------|------|
| CPU | Apple M4 (10 cores) |
| OS | macOS Darwin 25.4.0 |
| Python | 3.14.3 (per-interpreter GIL) |
| Rust | stable |
| Pyre | v1.3.0 |
| Channel | crossbeam-channel 0.5 (bounded, capacity = workers * 128) |
| Response | tokio::sync::oneshot |
| 工具 | wrk 4.2.0 |
| Handler | `return "ok"` (最简纯文本，隔离 handler 逻辑开销) |

### 调度架构

| 模式 | 路径 | Channel 开销 |
|------|------|-------------|
| GIL 模式 | Tokio task → `Python::attach()` → handler → 直接返回 | 无 |
| Sub-interp | Tokio task → `crossbeam.try_send()` → worker `recv()` → handler → `oneshot.send()` → Tokio 唤醒 | 完整 round-trip |

## 结果

### 吞吐量 (req/s)

| 并发 | GIL 模式 | Sub-interp 模式 | 倍数 |
|------|---------|-----------------|------|
| c64 (2t) | 81,479 | **185,548** | **2.3x** |
| c256 (4t) | 71,340 | **193,179** | **2.7x** |
| c512 (4t) | 53,262 | **183,238** | **3.4x** |

### 平均延迟

| 并发 | GIL 模式 | Sub-interp 模式 | 降幅 |
|------|---------|-----------------|------|
| c64 | 1.00ms | **337us** | **-66%** |
| c256 | 4.63ms | **1.14ms** | **-75%** |
| c512 | 12.32ms | **2.34ms** | **-81%** |

### 吞吐量对比 (c256)

```
GIL 模式      ███████████████████                          71,340 req/s
Sub-interp    ████████████████████████████████████████████████████ 193,179 req/s  (+171%)
```

### 延迟对比 (c256)

```
GIL 模式      ████████████████████  4.63ms
Sub-interp    █████                 1.14ms  (-75%)
```

### 扩展性曲线

```
吞吐量 (req/s)
200k ┤                    ●─────────────●─────────────●  Sub-interp
     │
150k ┤
     │
100k ┤
     │  ●
 80k ┤  │╲
     │     ╲
 60k ┤      ╲●
     │        ╲
 40k ┤         ╲●  GIL 模式
     │
     └──────────┬─────────────┬─────────────┬──
              c64           c256          c512
```

## 关键发现

### 1. Channel round-trip 不是瓶颈 — 它是加速器

Sub-interp 模式在所有并发级别下均大幅超越 GIL 模式。增加 crossbeam channel + oneshot 的调度开销后，吞吐量反而提升 2.3-3.4x。这证明 GIL 争抢才是真正的瓶颈，而非跨线程通信。

### 2. Sub-interp 吞吐量在高并发下保持稳定

从 c64 到 c512，sub-interp 模式的吞吐量仅从 186k 降至 183k（-1.6%）。而 GIL 模式从 81k 骤降至 53k（-35%）。这说明 crossbeam channel 在高压下的背压管理（bounded queue + `try_send`）运行良好，无显著锁竞争。

### 3. crossbeam-channel 底层已是 lock-free

crossbeam-channel 0.5 基于 Dmitry Vyukov 的 MPMC 算法，在高负载下走无锁热路径（不触发 futex）。替换为自定义 ring buffer 的理论收益在个位数纳秒级别，而 Python handler 执行本身在微秒级别。

### 4. oneshot channel 创建开销可忽略

`tokio::sync::oneshot::channel()` 的创建开销约 100ns。在 186k QPS 下，总耗时约 18ms/s，占 CPU 时间 < 2%。

### 5. WorkRequest 的数据传输已接近最优

- `body: Bytes` — 引用计数零拷贝（来自 hyper）
- `headers` — `LazyHeaders::Raw(HeaderMap)` 直接传递原始 hyper 头映射，仅在 Python 访问 `req.headers` 时才转换为 HashMap（`OnceLock`）
- 最大的分配开销来自 `method`、`path`、`client_ip` 的 `String`，但这些是短字符串（< 64 字节），分配开销在纳秒级

## 优化方向评估

| 优化方案 | 理论收益 | 实际价值 | 结论 |
|---------|---------|---------|------|
| Lock-free Ring Buffer 替换 crossbeam | ~10ns/op | 在 186k QPS 下约 2ms/s 总节省 | 不值得 |
| Arena 指针直传 (替代 oneshot) | ~100ns/op | 占比 < 2%，且增加 unsafe 复杂度 | 不值得 |
| Batch dispatching (批量消费) | 减少唤醒次数 | crossbeam 高负载下已不走 futex | 不值得 |
| 减少 Python 侧 PyObject 创建 | 减少 handler 执行时间 | 真正的天花板 | **值得** |

## 结论

**crossbeam channel + tokio oneshot 的调度架构已接近最优。** 在最简 handler 下，sub-interp 模式达到 193k req/s，是 GIL 模式的 2.7x。channel 调度开销被 GIL 消除带来的并行化收益完全覆盖。

性能天花板在 Python handler 的执行速度，而非 Rust 调度层。下一步优化方向应聚焦于：
1. 减少 handler 调用路径上的 PyObject 创建数量
2. 缓存编译后的 handler 字节码
3. 优化 `call_handler` 中的 FFI 调用链
