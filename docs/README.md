# Stross 文档

Stross 的设计与使用文档。建议阅读顺序：先 [requirements.md](requirements.md) 明确需求
（v2 定位下，roadmap 中的浏览器观看端等条目已被取代），再读 `architecture.md` 了解整体分层，
再按需查阅协议、插件化架构与平台指南。

| 文档 | 内容 |
|---|---|
| [requirements.md](requirements.md) | **需求文档 v2（唯一需求输入）**：一站式设备共享定位、决策记录、功能/非功能需求、阶段验收 |
| [architecture.md](architecture.md) | 五层模块化架构、数据流、Android 采集、历史设计决策 |
| [protocol.md](protocol.md) | 线上协议：24 字节 v2 帧头 + JSON 控制消息（能力协商 / 路由） |
| [plugin-architecture.md](plugin-architecture.md) | 插件化架构：可插拔传输层、内核控制面（设备图/会话/路由/鉴权）、四传输落地记录 |
| [platforms.md](platforms.md) | 平台构建与使用（Linux / Windows / Android），问题排查 |
| [roadmap.md](roadmap.md) | 路线图：已落地项与推迟项（WASM 策略插件 / 跨设备控制 / Sink 扩展等） |

## 快速入口

- 想跑起来：见根目录 [README](../README.md#快速开始桌面) 与 [platforms.md](platforms.md)
- 想改传输层：先读 [plugin-architecture.md](plugin-architecture.md) §4 与
  [`stross-transport`](../crates/stross-transport/src/lib.rs)
- 想改中继：见 [`stross-core/src/relay/`](../crates/stross-core/src/relay/mod.rs)
- 想改内核控制面：见 [`stross-app/src/kernel/`](../crates/stross-app/src/kernel/mod.rs)
- 想改 ffmpeg 管线：见 [`stross-media/src/pipeline/`](../crates/stross-media/src/pipeline/mod.rs)
