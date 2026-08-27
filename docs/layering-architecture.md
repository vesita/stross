# 分层架构（Layering）

> 状态：**已拍板并落实（第四轮协议/契约收敛 + 第五轮壳层去方言，2026-08 起）**。
> 本文件是分层的**决策依据与判据**——新增/迁移功能前先对照此表，避免再发生
> 「核心逻辑外溢到壳层」或「各平台各写一份连接流程」。

## 1. 总原则

**核心的功能必须平台无关。** 分层按「职责」而非「谁先写」划分；壳层（CLI /
GUI / Web 前端）只做**参数解析、展示、平台适配**，一切可复用的协议与编排
逻辑必须在库层（proto / core / app），经**库接口**调用。

```
stross-proto     线协议类型（纯数据，零依赖概念）        ← 类型放这里
stross-core      数据共享逻辑（中继/推流/观看/发现/HTTP 契约）  ← 服务端+客户端契约同层
stross-app       应用编排库（引导/协商/信任/订阅/文件传输）    ← 流程放这里
壳层（cli / gui）  参数解析 + 展示 + 平台适配（adb/ufw/ffmpeg 后端选择）
```

### 判据速查

| 内容 | 归属 | 禁止出现在 |
|---|---|---|
| 线协议消息 / 帧 / 序列化契约 | stross-proto | 壳层各自定义响应结构体 |
| 中继 HTTP API（服务端 + 客户端） | stross-core `relay::{http, client}` | 壳层/应用层手写 HTTP |
| 数据面（RelayClient / connect_watch / 会话） | stross-core | — |
| mDNS 发现 / IP 选址 | stross-core discovery / transport net | 壳层复刻过滤规则 |
| 广告 IP 决策 | stross-transport `net::advertise_ip` | 壳层自写 198.18/169.254 过滤 |
| 路径解析（XDG/HOME/app_data_dir） | stross-app `paths` | 壳层各写一份 |
| 引导 / 协商 / 信任 / 订阅握手编排 | stross-app `bootstrap` / `negotiator` / `subscriber` | CLI 命令实现流程 |
| 文件传输协议编排 | stross-app `file_xfer` | CLI 内联 |
| 采集 / 播放 / 平台能力 | stross-media + 壳层适配 | core |

**平台无关约束**：stross-core 零路径约定、零 OS 服务调用、零 UI 回调；
stross-app 代码本身平台无关（`Platform` 只是采集后端选择 hint，数据目录由
调用方注入）。平台适配只允许出现在壳层（CLI adb、GUI tauri/ufw、media 的
ffmpeg/cpal 后端）。

## 2. 收敛前的外溢清单（为什么会写本文件）

1. **中继 HTTP 契约 4 处手写**：`devices.rs::http_get<T>`（CLI）、
   `file_xfer.rs::http_get`（app，raw TCP）、`endpoint.rs::http_post_json`
   （CLI）、`discovery.ts fetchWithTimeout`（JS）——而 server 在
   `stross-core/relay/http.rs`。**响应类型定义在壳层 = 分层反转**：server 一改
   契约，CLI/JS 静默漂移（`/api/streams` 双形态兼容 hack 两处重复即为先兆）。
2. **订阅流程整个塞在 CLI**：`stross endpoint subscribe` 自带
   `LocalReceiver`（锚中继+建会话+自签凭证）、握手 HTTP、`advertise_ip`、
   「流尚未出现」重试——全是可与 GUI/手机共用的编排，却写成一条命令。
3. **advertise / 数据目录各两份**：`advertise_ip`（CLI endpoint.rs）与发现层
   选址同源却第三份实现；`base_dir` 在 serve.rs 与 endpoint.rs 重复。
4. **驱动安装靠壳层自觉**：`install_endpoint_driver` 由 serve.rs 手动调，
   GUI 桌面漏装 = 订阅了不推流。
5. **前端直接当协议客户端**：`discovery.ts`（670 行）含探测/聚合/过滤，
   `negotiate.ts`/`subscribe.ts` 直接 POST 18779。

## 3. 收敛落点（第四轮 + 第五轮完成）

| 收敛项 | 落点 |
|---|---|
| 协商线协议类型（ShareRequest/ShareGrant/RelayAddr/ShareTokenView）+ L2 目录（EndpointNode/EndpointDir） | `stross-proto::message::negotiator`（wire 逐字节兼容，单测锁定） |
| 中继 HTTP 客户端（info/streams/stream_watchers/post_json） | `stross-core::relay::client`（raw TCP 零新依赖） |
| 广告 IP / fake-IP 判定 | `stross-transport::net::{advertise_ip, is_fake_or_link_local}` |
| 订阅方编排（fetch_directory / subscribe_file / 重试 / 本地接收准备） | `stross-app::subscriber`；握手原语 `negotiator_client::request_grant`（CLI 与 GUI 命令同源） |
| 数据目录解析 | `stross-app::paths::data_dir` |
| 订阅驱动默认安装 | `bootstrap::start_handshake_on`（幂等；CLI serve 与 GUI 桌面行为一致） |
| 设备扫描聚合（mDNS + 探测 + 视图） | `stross-app::devices::{scan, probe_base, ScannedDevice}`（CLI devices / GUI scan_devices / adb 状态同源） |
| GUI 命令面（桥） | `apps/stross-gui/src-tauri/src/commands.rs`：scan_devices / probe_relay / anchor_streams / endpoint_ls / endpoint_subscribe / request_share_token |
| 前端 JS | **已去协议客户端化**：`discovery.ts`/`negotiate.ts`/`subscribe.ts` 不再 `fetch('/api/*')`，探测/握手/目录/等待流接入全部走 Rust 命令（手机 / 桌面 GUI / CLI 同库同命令） |
| CLI serve/devices/adb/endpoint | 全部改为调库接口，删本地实现；adb（660 行）拆 `adb/{mod,device,status,ui}` 子模块 |

## 4. 规则（改动前必读）

- **新增协议客户端一律进库层**：解析 `/api/*`（中继或协商端点）的代码只允许
  出现在 `stross-core::relay::client`（core 拥有服务端的 API）、
  `stross-app::subscriber` / `negotiator_client` / `negotiator`（app 拥有协商
  端点的 API），以及**把这些库函数暴露成 Tauri 命令的 commands.rs**。前端 JS
  禁止再出现 `fetch('/api/*')` 或手拼线协议 JSON。
- **新增线协议类型一律进 stross-proto**：壳层不得定义 wire 结构体
  （展示视图可以，如 `StreamView`）。
- **流程不写进 CLI 命令 / 前端 JS**：`stross endpoint subscribe` 这类多步编排
  应是对 app 库接口的一次调用 + 格式化；GUI 前端只渲染，流程经 Tauri 命令
  走同一个库函数。
- **平台无关红线**：core 出现路径/OS/平台分支即视为违规；app 出现平台分支
  必须说明（如防火墙自检仅限桌面）。壳层之间不产生"方言"：同一连接流程
  （握手 / 订阅 / 探测 / 等待接入）在所有平台只有一份实现。