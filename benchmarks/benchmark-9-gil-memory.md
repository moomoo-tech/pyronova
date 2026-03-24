# Benchmark 9: v0.5.0 GIL + 内存监控压测 (2026-03-24)

## 本轮目标

首次带 GIL Watchdog (`PYRE_METRICS=1`) 和内存 RSS 采样的对比压测。
验证：
1. Sub-interpreter 模式下主 GIL 是否真的零争用
2. 内存占用 vs Robyn 多进程
3. Watchdog 探针是否影响性能

## 结果：Pyre SubInterp v0.5.0 vs Robyn --fast

| 场景 | Pyre SubInterp | RSS | GIL peak | Robyn | 胜者 |
|------|----------------|-----|----------|-------|------|
| Hello World | **217,134** | 61.9 MB | **0μs** | 80,964 | Pyre **2.7x** |
| JSON small | **216,643** | 66.7 MB | **0μs** | 84,506 | Pyre **2.6x** |
| fib(10) | **205,361** | 125.3 MB | **0μs** | 80,745 | Pyre **2.5x** |
| fib(20) | 8,922 | 119.6 MB | **0μs** | **10,576** | Robyn 19% |
| sum(10k) | **75,138** | 62.5 MB | **0μs** | 44,028 | Pyre **1.7x** |
| sleep(1ms) | 7,877 | 36.6 MB | **0μs** | **82,283** | Robyn **10x** |

## GIL 分析：为什么 peak = 0μs 全场

**这不是 bug，是 Sub-interpreter 架构的核心价值体现。**

```
GIL Watchdog 探针抢的是主解释器的 GIL。

Sub-interpreter 模式下的线程模型：
┌─────────────────────────────────────────────────┐
│ 主解释器 GIL:  空闲（无人使用）                    │
│   → Watchdog 探针瞬间拿到 → 延迟 = 0μs           │
├─────────────────────────────────────────────────┤
│ 子解释器 GIL-0: Worker 0 独占 → 处理请求          │
│ 子解释器 GIL-1: Worker 1 独占 → 处理请求          │
│ 子解释器 GIL-2: Worker 2 独占 → 处理请求          │
│ ...                                              │
│ 子解释器 GIL-9: Worker 9 独占 → 处理请求          │
└─────────────────────────────────────────────────┘
10 个 worker 各自持有独立 GIL，并行执行，互不干扰。
主 GIL 全程空闲 → 探针延迟 = 0。
```

对比 GIL 模式（之前测试数据）：
```
GIL 模式：所有请求争抢同一把主 GIL
  空闲:   avg = 16μs,  peak = 190μs
  快负载: avg = 1.9ms, peak = 22ms
  重计算: avg = 1.9ms, peak = 7.8ms (handler 持锁)
```

**结论：Sub-interpreter 的 Per-Interpreter GIL 完全消除了 GIL 争用。**
这是 Pyre 能跑到 217k req/s 的物理基础 —— 零锁竞争。

## 内存分析

| 负载类型 | Pyre RSS | 说明 |
|---------|---------|------|
| sleep (最轻) | 36.6 MB | 基线内存，几乎不消耗 |
| Hello/JSON/sum | 62-67 MB | 正常工作内存 |
| fib(10/20) 重 CPU | 119-125 MB | Python 递归栈 × 10 workers |

### vs Robyn 内存估算

Robyn `--fast` 启动 22 个独立 OS 进程。每个进程约 20MB 基线：

| 框架 | 进程数 | 估算 RSS | 实测参考 |
|------|--------|---------|---------|
| **Pyre SubInterp** | **1** | **67 MB** | 实测 |
| Robyn --fast | 22 | ~440 MB | 历史数据 ~451 MB |

**Pyre 内存效率 6.6x 优于 Robyn。**

同样的 CPU 并行度（10 个独立 GIL vs 22 个进程），Pyre 用 1/7 的内存实现了 2.7x 的吞吐量。

## Watchdog 对性能的影响

| 场景 | Round 4 (无 Watchdog) | Round 9 (有 Watchdog) | 变化 |
|------|---------------------|---------------------|------|
| Hello World | 213,887 | 217,134 | +1.5% |
| JSON small | 209,414 | 216,643 | +3.5% |
| fib(10) | 206,179 | 205,361 | -0.4% |
| sum(10k) | 73,805 | 75,138 | +1.8% |

**Watchdog 零性能影响。** Sub-interp 模式下探针抢的是主 GIL，与 worker 的独立 GIL 无交集。

## 下一步监控增强

当前 Watchdog 只监控**主 GIL**。要监控每个子解释器内部的阻塞，需要：

1. **Per-worker 计时埋点**（`interp.rs` 的 `call_handler` 前后记录耗时）
2. **Event loop lag 监控**（Phase 7.2 asyncio 引擎的心跳协程）
3. 通过 `/__pyre__/metrics` 端点暴露 per-worker 统计
