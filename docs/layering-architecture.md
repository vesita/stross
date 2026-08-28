# 分层架构（Layering）

> 状态：**统一化重构完成（第七轮）**。本轮把内核定义落定：`stross-core` 更名
> `stross-kernel` 并吸收原 `stross-app` 的全部服务逻辑；平台适应独立成
> `stross-bridge` 桥接层；壳层只剩参数解析 + 展示 + 平台适配。
> 本文件是分层的**决策依据与判据**——新增/迁移功能前先对照此表。

## 1. 分层总览

依赖方向自底向上、单向无环：

```text
┌──────────────────────────────────────────────────────────────────┐
│ 壳层：apps/stross-cli · apps/stross-gui（桌面+Android）· apps/stross-relay │
│   参数解析 + 展示 + 平台适配（adb / ufw / ffmpeg-cpal 后端选择）        │
├──────────────────────────────────────────────────────────────────┤
│ stross-bridge   平台适应桥接层：paths（数据目录）/ hostname / 平台端点构造 │
├──────────────────────────────────────────────────────────────────┤
│ stross-kernel   ★ 内核：全部平台无关服务，单一门面 Kernel                  │
│   数据面 relay{srv,client} · sender/watch/jitter · discovery(mDNS)     │
│   信令 control{srv,client} · negotiator{srv,client} · subscriber        │
│   file_xfer · bootstrap · devices(扫描聚合) · engine(推流) · receiver(接收)│
│   kernel{会话/路由/鉴权/凭证/端点/设备图} · view(展示视图构造)              │
├──────────────────────────────────────────────────────────────────┤
│ stross-types   应用契约层：跨壳层类型单一真源（展示视图 + 控制面载荷 +      │
│                DTO：AppInfo/StatusView/CtrlPayload/CameraDevice…）      │
├──────────────────────────────────────────────────────────────────┤
│ stross-media   能力层：CaptureBackend / PlaybackSink / StreamConfig /    │
│                设备枚举/采集播放抽象（ffmpeg/cpal 后端实现）             │
├──────────────────────────────────────────────────────────────────┤
│ stross-transport  传输抽象 + 传输插件（ws/srt/quic/webrtc/memory）+        │
│                RelayUrl + net（local_ips / advertise_ip / fake-IP 判定）│
├──────────────────────────────────────────────────────────────────┤
│ stross-proto   线协议类型（消息/帧/时间，纯数据）                          │
└──────────────────────────────────────────────────────────────────┘
```

| 层 | crate | 职责 | 依赖 |
|---|---|---|---|
| 协议 | `stross-proto` | 线上契约：帧头 + 控制消息 + 协商握手 + L2 目录（`message/negotiator.rs`）+ 枚举 wire 字符串（`as_str`/`from_wire`） | 无内部依赖 |
| 传输 | `stross-transport` | `Transport`/`DataSession` 抽象 + ws/webrtc/srt/quic/memory 实现 + RelayUrl + 本机 IP | proto |
| 能力 | `stross-media` | 采集（ffmpeg 后端）/ 播放（PlaybackSink/cpal）/ 管线 StreamConfig / 设备枚举（`CameraDevice` 收敛至 types，此处重导出） | proto + types |
| **契约** | **`stross-types`** | **跨壳层应用契约单一真源**：展示视图（AppInfo/RelayInfo/StreamStatus…）+ 控制面载荷（CtrlResponse::Ok payload）+ DTO（CameraDevice/PendingRequest/MediaSubscribeOutcome/ShareTokenView） | proto |
| **内核** | **`stross-kernel`** | **全部平台无关服务**（见 §2），以 [`Kernel`] 门面为单一入口；`pub use stross_types::*` 保持壳层路径兼容 | proto + transport + media + types |
| 桥接 | `stross-bridge` | 平台适应：paths / hostname / 平台端点构造（load 探测注入，只产出**参数**，不持有状态） | kernel + media |
| 壳层 | `stross-cli` / `stross-gui` / `stross-relay` | 参数解析 + 展示 + 平台适配 | kernel + bridge + media + types |

## 2. 内核定义（第七轮落定）

**内核 = `stross-kernel` crate = 所有平台无关的服务提供。** 没有第二个
"内核"：原 `stross-app::kernel::Kernel`（会话/路由骨架）与原
`stross-app::StrossApp`（运行态状态机）已合并为唯一的 [`Kernel`] 门面。

内核提供（壳层只调这些接口，禁止复刻实现）：

| 服务 | 模块 | 说明 |
|---|---|---|
| 数据面 | `relay`（server + client + http + peers + data_plane + state）、`sender`、`watch`、`session_channel`、`jitter` | 中继服务端与客户端契约同层（单一真源）、推流/观看链路、抖动缓冲 |
| 发现 | `discovery`（mDNS 浏览/广播）、`devices::scan_lan` | 设备发现 + 扫描聚合一站式（mDNS + 探测 + 手动地址去重） |
| 信令 | `control`（CtrlServer + client）、`negotiator` + `negotiator_client`、`subscriber`、`file_xfer`、`bootstrap` | 控制面/协商端点（服务端 + 客户端）、订阅方编排、文件端点传输、引导层 |
| 端点框架 | `kernel::endpoint`（EndpointRegistry + Endpoint 契约）、`kernel::graph`、`kernel::session`、`kernel::auth` | 单层端点（load/share 契约，端点自驱动）、设备图、会话/路由、PIN 鉴权 |
| 编排 | `kernel`（Kernel 门面）、`engine`（SenderEngine）、`receiver`（Receiver） | 状态机、推流引擎、接收编排（在 `stross-media` 能力之上） |
| 展示 | `view`（构造助手）+ `stross-types`（类型） | 跨壳层复用的展示视图构造（relay_info/watch_urls）；类型定义在 stross-types，壳层不定义响应结构体 |
| 端口 | `relay::{DEFAULT_PORT=18777, GUI_PORT=8777}`、`DEFAULT_CTRL_PORT=18778`、`DEFAULT_NEGOTIATOR_PORT=18779`、`DEFAULT_SRT_PORT=33462`、`DEFAULT_QUIC_PORT=33464` | 全仓端口字面量清零（壳层/前端一律引用库常量） |

**事件面**：所有变更经 `Kernel::subscribe()` → [`KernelEvent`] 广播（会话/路由/
数据面流生命周期），UI 订阅代替轮询。

## 3. 平台适应桥接层（第七轮新增）

**平台知识只允许出现在 `stross-bridge` 与壳层。** 桥接层产出参数、不持有状态：

| 桥接项 | 模块 | 注入目标 |
|---|---|---|
| 数据目录解析（XDG/HOME 回退链） | `bridge::paths::data_dir` | `ensure_identity` / 身份与信任清单 |
| 本机主机名（OS 调用收敛点） | `bridge::hostname::hostname_or` | `Discovery::start` / `start_relay_fixed` / `ensure_identity` |
| 平台端点构造（桌面/Android 能力清单 + load 探测闭包） | `bridge::devices::platform_endpoints` + `seed_platform_endpoints` | `Kernel::seed_endpoint` |

壳层启动样板（CLI serve / GUI 桌面 / Android 三处共用同一套原语）：
`Kernel::new(bridge::devices::platform())` → `set_backend` → `seed_platform_endpoints`
→ `bootstrap::ensure_identity(base, hostname)` → `bootstrap::start(...)`。

## 4. 判据速查

| 内容 | 归属 | 禁止出现在 |
|---|---|---|
| 线协议消息 / 帧 / 序列化契约 | stross-proto | 壳层各自定义响应结构体 |
| 中继 HTTP API（服务端 + 客户端） | stross-kernel `relay::{http, client}` | 壳层手写 HTTP |
| 数据面（RelayClient / connect_watch / 会话） | stross-kernel | — |
| mDNS 发现 / IP 选址 / 广告 IP 决策 | stross-kernel discovery / stross-transport net | 壳层复刻过滤规则 |
| 控制面（服务端 + 客户端） | stross-kernel `control` | CLI 手写 WS 客户端 |
| 协商端点 / 订阅握手 / 信任清单 | stross-kernel `negotiator` / `negotiator_client` / `subscriber` | CLI 命令实现流程 |
| 文件传输协议编排 | stross-kernel `file_xfer` | CLI 内联 |
| 端点框架（EndpointRegistry / 会话 / 路由 / 鉴权） | stross-kernel `kernel::*` | — |
| 推流 / 接收编排 | stross-kernel `engine` / `receiver` | 壳层各写一份 watch→通道→播放 |
| 数据目录解析 / 主机名 / 平台端点构造 | stross-bridge | 壳层各写一份 XDG/HOME 回退链 |
| 采集 / 播放 / 平台能力 | stross-media + 壳层适配 | kernel（能力交付型 `cfg` 除外） |
| 端口常量 | stross-kernel | 壳层/前端硬编码端口 |
| 展示视图类型 / 控制面载荷 / 共享 DTO | stross-types | 壳层定义 wire 结构体（类型单一真源在契约层） |

## 5. 红线（改动前必读）

- **内核零路径约定、零 OS 服务调用、零平台分支**：`stross-kernel` 不允许出现
  XDG/HOME 回退链、`hostname::get()`、`cfg(target_os="android")` 逻辑分支。
  数据目录 / 主机名 / 平台设备一律由调用方（壳层经 bridge）**注入**。
  *例外*：`start_receive` 的一处 `cfg(not(android))` 属播放能力可用性交付
  （cpal 音频输出仅桌面），不是逻辑分支。
- **新增协议客户端一律进库层**：解析 `/api/*`（中继或协商端点）的代码只允许
  出现在 `stross-kernel`（`relay::client` / `negotiator` / `negotiator_client` /
  `subscriber` / `control::client`），以及把这些库函数暴露成 Tauri 命令的
  `commands.rs`。前端 JS 禁止再出现 `fetch('/api/*')` 或手拼线协议 JSON。
- **新增线协议类型一律进 stross-proto**：壳层不得定义 wire 结构体
  （展示视图可以，如 `StreamView`）。
- **流程不写进 CLI 命令 / 前端 JS**：多步编排应是对内核接口的一次调用 +
  格式化；GUI 前端只渲染，流程经 Tauri 命令走同一个内核函数。
- **同一连接流程只有一份实现**：握手 / 订阅 / 探测 / 等待接入 / 设备枚举
  在所有平台（CLI / 桌面 GUI / Android）只有一份实现，平台差异在 bridge 与
  壳层适配（采集后端 / 播放后端 / 防火墙 / adb）。
- **桥接层只产出参数**：`stross-bridge` 不持有状态、不启动服务、不定义协议；
  它把平台知识翻译成内核能收的参数（base_dir / hostname / 端点与探测闭包）。

## 6. 收敛史

| 轮次 | 收敛项 |
|---|---|
| 第四轮 | 协议/契约收敛：协商线协议类型进 proto；中继 HTTP 客户端进 core；advertise/fake-IP 判定进 transport net |
| 第五轮 | 壳层去方言：订阅编排进 app::subscriber；数据目录进 app::paths；驱动默认安装；扫描聚合一站式；GUI 命令薄化；前端去协议客户端化 |
| 第六轮 | 端口真源（DEFAULT_PORT=18777 / GUI_PORT=8777）；控制面客户端进库层；CLI receive 走 Receiver；传输层去 axum 依赖；core 零 OS 调用（hostname 注入） |
| **第七轮** | **统一化**：core 更名 kernel 并吸收 app 全部服务 → 单一 [`Kernel`] 门面；平台适应独立 `stross-bridge`；壳层只做参数解析 + 展示 + 平台适配；旧文档推倒重写 |
