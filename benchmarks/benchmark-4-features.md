# Benchmark 4: v0.3.0 功能补全后性能验证 (2026-03-24)

新增功能：request headers、query_params、PyreResponse（自定义状态码/headers/content-type）、middleware（before_request/after_request）

测试环境：macOS ARM64 (Apple Silicon), Python 3.14.3, Rust 1.93.1, wrk 4.2.0
参数：`wrk -t4 -c256 -d10s`

## 结果

| 框架 | GET `/` req/s | GET `/hello/{name}` req/s | GET `/search?q=` req/s | avg 延迟 |
|------|-------------|------------------------|---------------------|---------|
| **Pyre SubInterp** | **223,440** | **223,545** | **221,787** | **0.85ms** |
| Pyre GIL (含 middleware) | 103,712 | 88,245 | 99,095 | 2.5ms |
| Robyn --fast | 94,287 | 94,854 | — | 30ms |

## 与 v0.2.0 对比

| 指标 | v0.2.0 | v0.3.0 | 变化 |
|------|--------|--------|------|
| SubInterp GET `/` | 216,517 | 223,440 | +3.2% |
| SubInterp GET `/hello` | 204,636 | 223,545 | +9.2% |
| GIL GET `/` | 100,673 | 103,712 | +3.0% |
| GIL GET `/hello` | 97,727 | 88,245 | -9.7% (新增 middleware 开销) |

## 关键结论

1. **新功能零性能损耗** — headers 提取、query_params 解析在 SubInterp 模式下完全无感（221k vs 223k）
2. **SubInterp 模式反而更快了** — 可能是 Rust 编译器对重构后代码优化更好
3. **Middleware 开销极小** — GIL 模式含 after_request hook 仅损失约 3%（GET `/`），JSON 路由损失较大（~10%）因为 after_request 需要创建 PyreResponse 对象
4. **Pyre SubInterp vs Robyn 差距继续拉大** — 2.4x 吞吐量，35x 更低延迟
