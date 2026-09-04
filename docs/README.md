# Stross 文档总览（docs 引导）

本目录是 Stross 的全部设计/规格/记录。**入口顺序**：

1. **AI 代理**：先读根目录 `AGENTS.md`（协作纪律 + 已知坑 + 文档指针）→ 按需查本表；
2. **新读者**：先 [requirements.md](requirements.md)（需求）→ [framework-v3.md](framework-v3.md)（v3 唯一架构源）→ 按任务查下表；
3. **改代码前**：对照 [framework-v3.md](framework-v3.md)（红线 + 模块边界）+ 本表「权威源」列。

## 文档清单（职责 + 状态）

| 文档 | 职责 | 状态 |
|---|---|---|
| [AGENTS.md](../AGENTS.md) | **AI 协作常驻指南**：分层铁律 / 端口 / 构建 / 真机套路 / 坑 / 纪律 | 权威（根目录） |
| [framework-v3.md](framework-v3.md) | **v3 框架定稿（唯一架构源）**：八概念 crate（节点/端点/共享/订阅/传输/序列化/pick/发现）+ 策略注册表模式 + 模块边界 + 实施阶段 | 权威（重构完成） |
| [requirements.md](requirements.md) | 需求 v2（唯一需求输入） | 权威 |
| ~~layering-architecture.md~~ | 分层判据（v2；已并入 framework-v3.md §2，文件已删除） | 已删除 |
| ~~architecture.md~~ | 架构总览（v2；已并入 framework-v3.md，文件已删除） | 已删除 |
| ~~endpoint-model-v2.md~~ | 端点框架 v2 规格（已并入 framework-v3.md §3.2，文件已删除） | 已删除 |
| ~~endpoint-model.md~~ | 端点框架 v1 规格（已被 v2 演进取代） | 已删除 |
| ~~plugin-architecture.md~~ | 可插拔传输基座（v2；已并入 framework-v3.md §3.5，文件已删除） | 已删除 |
| ~~comm-mode-v2.md~~ | 通信模式 v2（已并入 framework-v3.md §3.6/§3.7，文件已删除） | 已删除 |
| [protocol.md](protocol.md) | 线上协议：24 字节 v2 帧头 + JSON 控制消息 | 权威 |
| [android-build.md](android-build.md) | **Android 构建实测固化（JDK/工具链/SDK/网络坑 唯一真源）** | 权威 |
| [platforms.md](platforms.md) | 平台构建与使用（Linux/Windows/Android 快速路径） | 权威（JDK 详见 android-build） |
| [roadmap.md](roadmap.md) | 路线图（已完成项、架构演进与后续待办） | 权威 |
| [iteration-plan.md](iteration-plan.md) | 迭代日志（轮次索引 + 待办排期；详细记录见 git 历史） | 记录 |
| [dev-playbook.md](dev-playbook.md) | AI 速查卡：构建时序坑 / 前端约定 / 真机套路 / 门禁 / FSM 状态机与 Android 播放规范 | 记录（压缩上下文用） |
| [../dev-notes/README.md](../dev-notes/README.md) | **会话踩坑原料库**：真实排查过程、性能瓶颈定位与根因归档 | 记录 |
| ~~stress-test-report.md~~ | 压力测试记录（已并入历史，见 git） | 已删除 |
| ~~mdns-android-finding-debug.md~~ | mDNS 排查记录（结论已并入 AGENTS.md §6 / dev-playbook §5） | 已删除 |
| ~~closed-loop-plan.md~~ | 运行闭环整改计划（P0-1 已落地；未决项并入 iteration-plan 排期） | 已删除 |

## 快速入口（按任务）

- 想跑起来：根目录 [README](../README.md#快速开始桌面) + [platforms.md](platforms.md)
- 想改分层 / 判断新功能归属：先读 [framework-v3.md](framework-v3.md) §2（八概念 crate 边界）
- 想改端点框架 / 新增数据源：先读 [framework-v3.md](framework-v3.md) §3.2，实现见 `stross-endpoint`
- 想改传输层 / 增加传输：先读 [framework-v3.md](framework-v3.md) §3.5（`stross-transport`）
- 想改序列化 / pick 规则：先读 [framework-v3.md](framework-v3.md) §3.6/§3.7（`stross-serialize` / `stross-pick`）
- 想改发现：先读 [framework-v3.md](framework-v3.md) §3.8（`stross-discovery`）
- 想改中继：`stross-kernel/src/relay/server.rs`
- 想改内核门面：`stross-kernel/src/kernel/mod.rs`
- 想改 ffmpeg 管线：`stross-endpoint/src/pipeline/`
- 想改跨壳层类型：`stross-view`
- 想改 Android 构建：先读 [android-build.md](android-build.md)（唯一真源，AGENTS.md/platforms 只引用）
- 压缩对话 / 恢复上下文：读 [dev-playbook.md](dev-playbook.md)
- 查阅历史踩坑与性能调优过程：读 [dev-notes/](../dev-notes/README.md)

## 文档维护规则（去重与演进）

1. **单一真源**：同一事实只在一处详述，其余文档只写指针。示例：
   - Android 构建/JDK → `android-build.md`（AGENTS.md、platforms.md 只引用）；
   - 分层判据 → `layering-architecture.md`；端点规格 → `endpoint-model-v2.md`（v1 已删除，历史见 git）；
   - 传输基座 → `plugin-architecture.md`；通信模式演进 → `comm-mode-v2.md`（只写增量，不重复传输基座）。
2. **新文档先挂表**：新增文档必须在本表登记（职责 + 状态），并在对应权威文档顶部加关联链接。
3. **状态标注**：设计提案（待实施）与已落地/归档明确区分；落地方案状态写进文档头部。
4. **术语统一**：用户可见交互统一「共享/订阅」；**不用「通告/广播」**（与 mDNS 广播歧义）；交付字段等系统细节不进用户文案。
5. **AGENTS.md 是最上层**：AI 协作先读它；文档指针的增删改要同步到 AGENTS.md 相关条目。
