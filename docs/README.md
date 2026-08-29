# Stross 文档

Stross 的设计与使用文档。建议阅读顺序：先 [requirements.md](requirements.md) 明确需求
（v2 定位下，roadmap 中的浏览器观看端等条目已被取代），再读
[layering-architecture.md](layering-architecture.md) 了解分层判据（当前架构的
权威入口），随后按需查阅架构、端点框架、协议与平台指南。

| 文档 | 内容 |
|---|---|
| [requirements.md](requirements.md) | **需求文档 v2（唯一需求输入）**：一站式设备共享定位、决策记录、功能/非功能需求、阶段验收 |
| [layering-architecture.md](layering-architecture.md) | **分层判据**：proto → transport → endpoint → types → kernel → bridge → 壳层；内核定义与红线（改代码前先对照） |
| [architecture.md](architecture.md) | 分层架构总览、交互模型（通告/订阅）、数据流、中继/接收链路、关键设计决策 |
| [endpoint-model.md](endpoint-model.md) | **端点框架规格（唯一规格源）**：节点→端点单层模型、load/share 行为契约、目录/订阅握手、wire v3 |
| [protocol.md](protocol.md) | 线上协议：24 字节 v2 帧头 + JSON 控制消息（能力协商 / 路由 / 协商握手） |
| [plugin-architecture.md](plugin-architecture.md) | 插件化架构（历史设计）：可插拔传输层、内核控制面、四传输落地记录 |
| [platforms.md](platforms.md) | 平台构建与使用（Linux / Windows / Android），问题排查 |
| [roadmap.md](roadmap.md) | 路线图：P0 设备网格（已完成）、P2 跨设备推流（已完成）、P3 AV 同步 / P4 跨网段路由 / 二期无损共享（待办） |
| [iteration-plan.md](iteration-plan.md) | 迭代日志：阶段 A/B（已收口）+ 第七~十一轮重构记录、阶段 C/D/E 待办 |
| [android-build.md](android-build.md) | Android 构建实测固化（工具链 / SDK 许可证 / 网络坑 / 真机验证锚点） |
| [stress-test-report.md](stress-test-report.md) | 压力测试记录（长跑/弱网基线，含 SRT 调参前数据） |

## 快速入口

- 想跑起来：见根目录 [README](../README.md#快速开始桌面) 与 [platforms.md](platforms.md)
- 想改分层 / 判断新功能归属：先读 [layering-architecture.md](layering-architecture.md) §4 判据速查
- 想改传输层：先读 [plugin-architecture.md](plugin-architecture.md) §4 与
  [`stross-transport`](../crates/stross-transport/src/lib.rs)
- 想改中继：见 [`stross-kernel/src/relay/`](../crates/stross-kernel/src/relay/server.rs)
- 想改内核门面：见 [`stross-kernel/src/kernel/`](../crates/stross-kernel/src/kernel/mod.rs)
- 想改端点框架 / 新增数据源：先读 [endpoint-model.md](endpoint-model.md)，实现见
  [`stross-endpoint`](../crates/stross-endpoint/src/lib.rs) 与
  [`stross-kernel/src/kernel/endpoint.rs`](../crates/stross-kernel/src/kernel/endpoint.rs)
- 想改 ffmpeg 管线：见 [`stross-endpoint/src/pipeline/`](../crates/stross-endpoint/src/pipeline/mod.rs)
- 想改跨壳层类型：见 [`stross-types`](../crates/stross-types/src/lib.rs)
