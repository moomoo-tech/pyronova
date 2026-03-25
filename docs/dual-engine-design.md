# 双引擎架构设计：Tokio + io_uring (Monoio)

> 状态：设计阶段，等 Linux 机器到位后实施

## 核心思路

一套业务代码，两套底层引擎，通过 Cargo feature flags 编译期切换：

```
pip install pyreframework          → Tokio (macOS/Linux/Windows，生态拉满)
pip install pyreframework[uring]   → Monoio (Linux only，极致性能)
```

## 两条路线对比

| 维度 | Tokio (当前) | Monoio (io_uring) |
|------|-------------|-------------------|
| I/O 模型 | epoll + Work-stealing | io_uring + Thread-per-core |
| 线程模型 | Future 跨线程飘 (Send+Sync) | Shared-nothing (单核单线程) |
| 系统调用 | 频繁 syscall (read/write/epoll_wait) | Ring buffer 批量提交，极少 syscall |
| 生态 | hyper/reqwest/全 Rust 异步生态 | 需造轮子或找兼容库 |
| 平台 | macOS/Linux/Windows | Linux 5.1+ only |
| 与子解释器契合 | 需要跨线程调度 | **天作之合**：1 core = 1 thread = 1 sub-interp |

## 为什么 Monoio + 子解释器是天作之合

```
Monoio Thread-per-core:
  CPU 0 → Monoio thread 0 → Sub-interpreter 0 → Arena pool 0
  CPU 1 → Monoio thread 1 → Sub-interpreter 1 → Arena pool 1
  ...
  CPU 9 → Monoio thread 9 → Sub-interpreter 9 → Arena pool 9

零跨核锁，零线程切换，零 IPC。
数据从网卡 DMA → io_uring ring buffer → Rust Arena → memoryview → Python。
```

## 实现方案：Cargo Feature Flags

```toml
# Cargo.toml
[features]
default = ["tokio_engine"]
tokio_engine = ["tokio", "hyper", "hyper-util"]
uring_engine = ["monoio"]

[dependencies]
tokio = { version = "1", features = ["full"], optional = true }
monoio = { version = "0.2", optional = true }
```

```rust
#[cfg(feature = "tokio_engine")]
pub async fn start_server(addr: SocketAddr) { /* hyper + tokio */ }

#[cfg(feature = "uring_engine")]
pub fn start_server(addr: SocketAddr) { /* monoio thread-per-core */ }

// 共享：与引擎无关的 Python 调度逻辑
pub fn dispatch_to_interpreter(arena_ptr: usize) { /* ... */ }
```

## 实施计划

| 阶段 | 内容 | 前置条件 |
|------|------|---------|
| 1 | Tokio 引擎完善（当前） | ✅ 已完成 |
| 2 | 抽象 I/O trait 层 | 设计统一接口 |
| 3 | Monoio 引擎 PoC | Linux 机器 |
| 4 | Feature flag 切换 | 两个引擎都验证 |
| 5 | `pip install pyreframework[uring]` | CI/CD 支持 |

## 当前决策

**先用 Tokio 打天下。** 理由：
1. 当前 220k QPS 瓶颈在 Python handler 执行，不在 I/O 系统调用
2. macOS 开发环境无法跑 io_uring
3. Tokio 生态 (hyper/tungstenite) 已验证稳定
4. io_uring 收益在 >500k QPS 或微秒级延迟场景才显著

**等 Linux 生产机器到位后，开 uring 分支。**
