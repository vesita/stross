# 端点框架 · 设计规格（节点 → 设备 → 端点）

> 状态：**讨论定稿；P1 已提交（2f22f21）、第二轮已提交（ab5dd9b）、第三轮
> 已完成待提交**（文件端点 TRACK_FILE 传输、订阅→推流联动、端点命令、
> 本地双端小文件互发测试、TXT 拆多 key 修复）。待办：GUI 前端渲染与公开/订阅
> 交互、目录互斥锁、真机回归、Private 白名单动态过滤、publish 时 mDNS 重广播。
> 对齐 `iteration-plan.md` 阶段 A/B；P1 范围：媒体类端点、一设备一端点（1:1）、
> 双向 delivery、主程序代持目录。
> 本文件是协议与实现的唯一规格源；术语定稿见 §1，任何讨论改动须先改这里。

---

## 1. 术语（定稿）

| 层级 | 术语 | 英文 | 定义 |
|---|---|---|---|
| L0 | **节点** | Node | 物理机器上的一个 Stross 运行实例（手机、电脑）；**mDNS 广播单位**，有 IP、有持久身份（`identity.json` 的 device_id） |
| L1 | **设备** | Device | 节点上**持久存在**的能力实体：摄像头、麦克风、屏幕、系统声音、文件、服务……与订阅与否无关，一直存在，有稳定 id |
| L2 | **端点** | Endpoint | 设备**被公开后**形成的订阅入口实例：携带协议选择（公开者选）、可见性、delivery、运行状态；有生命周期（通告/取消通告）。P1 为 1:1（一设备至多一个公开端点） |
| — | 引导层 | Bootstrap | 发现（节点级 mDNS）+ 目录（节点→设备→端点）+ 订阅握手；不是独立进程，是每节点自带的逻辑层 |
| — | 发现 / 通告 / 订阅 / 推流 | Discovery / Publish / Subscribe / Push | 发现=节点广播/浏览；通告=设备实例化为端点；订阅=对端点的意图登记→握手→凭证→数据面；推流=订阅成立后的传输，沿用 watch/push 双路径 |

协议 wire 字段统一英文 `nodeId / deviceId / endpointId`（与现有 `stream_id` 风格一致）；
本表中文术语只管 UI 与文档。UI：原"设备卡片"改称**节点卡片**，卡片内列**设备**。

---

## 2. 三层模型与数据流

```
节点 A（手机）                    节点 B（电脑）
  ├─ 设备 mic:builtin  ◄──公开──►  端点（1:1，协议/可见性/delivery/状态）
  ├─ 设备 screen:0                 目录持有者 = 节点（每节点一个）
  └─ 设备 camera:0
```

**Pull 流（订阅者连公开者）** —— 订阅 B 的屏幕：
B 公开「屏幕」端点（idle，无订阅）→ A 发现节点 B → 拉端点详情 → 订阅
（Public/Confirm/Private 决策）→ 授予接入 → B 收到订阅事件**自动建会话并启动采集/推流**
→ A 连 B 中继 watch（现有 `Watch` 路径零改动）。

**Push 流（公开者连订阅者）** —— 订阅 B 的麦克风（反向外设）：
B 公开「麦克风」端点 → A 订阅时在请求里附带**自己的中继地址**（`/api/info` 的
ws/srt/quic 端口）→ 授予凭证（现有 `ShareToken`）→ A 侧中继等待 → B 凭凭证出站
push 到 A 中继（现有 `Hello + share_token` 路径零改动）。

**采集生命周期解耦**：端点可常驻 Idle；订阅达成才建会话、启动采集/编码；无订阅者
不采集（省电/省资源）。`startOnPublish`（一通告就推、后到订阅者连看）列入 P1 后。

---

## 3. 数据模型（stross-proto 扩展）

全部新增字段带 `#[serde(default)]` / `skip_serializing_if`，旧端解析忽略未知字段，
wire 格式稳定（`rename_all` 与现状一致）。

### 3.1 设备种类

扩展 `MediaKind`，追加两个占位变体（二期 E 与后续启用）：

```rust
pub enum MediaKind {
    Screen, Window, Camera, Mic, SystemAudio, Input, Clipboard,
    File,        // 二期 E：文件互传
    Service,     // 后续：程序服务端点（schema 未定，仅占位）
}
```

### 3.2 设备与摘要

```rust
/// 目录详情层（L2，GET /api/endpoints）用。
pub struct DeviceInfo {
    pub device_id: String,   // 节点内稳定 id："mic:builtin" / "screen:0" / "camera:front"
    pub kind: MediaKind,
    pub name: String,        // 用户可见："内置麦克风"（P1 用默认名，改名后置）
    pub builtin: bool,       // 随节点常驻（静态枚举）或动态加入
}

/// mDNS 摘要层（L1，DiscoveryInfo v2）用——只带 id/kind/name/是否已公开，绝无详情。
pub struct DeviceSummary {
    pub device_id: String,
    pub kind: MediaKind,
    pub name: String,
    pub published: bool,
}
```

### 3.3 端点清单（公开方协议/可见性/状态的唯一来源）

```rust
pub enum Visibility {
    Public,                                  // 任何人可订阅，免确认
    Confirm,                                 // 首见人工确认；已信任节点自动（复用 TrustStore）
    Private { nodes: Vec<String> },          // 仅白名单节点（按节点 device_id）
}

pub enum Delivery { Pull, Push, Both }       // 数据面连接方向，见 §2

pub struct TransportPreference {
    pub transport: TransportId,              // 复用现有枚举 Ws/WebRtc/Srt/Quic/Memory
    pub priority: u8,                        // 0 = 公开者最优先
}

pub struct EndpointManifest {
    pub endpoint_id: String,                 // 全局：节点内唯一短 id（1:1 时 = device_id）
    pub device: DeviceInfo,
    pub visibility: Visibility,
    pub delivery: Delivery,
    pub transports: Vec<TransportPreference>,// 公开者选择的协议（按优先序）
    pub codecs: Vec<CodecId>,                // 该端点实际可用的编解码
    pub state: EndpointState,                // idle | active | suspended
    pub subscribers: u32,                    // 当前订阅数（= 关联会话 watchers/sinks）
    pub updated_at: u64,
}
```

> 协议选择的正确性由 `ReliabilityProfile` 兜底：文件/剪贴板/服务 → Lossless
> （QUIC/WS）；屏幕/音频 → Lossy/Adaptive（SRT/QUIC/WebRTC）。公开者按端点类型
> 选协议，订阅者只在 `transports` 列表内协商/降级（复用 `Offer`/`Answer`）。

### 3.4 DiscoveryInfo v2（L1 摘要）

```rust
pub struct DiscoveryInfo {
    pub v: u8,                               // 2
    pub name: String,
    pub roles: Vec<RoleId>,                  // 不变
    pub media: Vec<MediaKind>,               // 不变（设备级能力总和，兼容旧端）
    pub transports: Vec<TransportId>,        // 不变
    pub codecs: Vec<CodecId>,                // 不变
    #[serde(default)]
    pub devices: Vec<DeviceSummary>,         // 新增：设备清单摘要
}
```

**TXT 上限风险（§11.1，第三轮已实测并修复）**：mDNS TXT 单条 character-string
≤ 255B（RFC 1035，mdns-sd 强校验）。实测整包 JSON（base ≈200B + 3 台设备摘要
≈270B → **449B**）广播直接失败（`TXT property length 449 exceeding 255`）。
采用 **方案 b：拆多 key**（2026-08-27 拍板）：
- `stross` key：基础能力（base ≤255B，恒不含 `devices`）；
- `dev.<n>` key：每台设备一个摘要（≤255B）；
- v1 端只读 `stross` key（忽略未知 key）→ 双向兼容，实测通过。

### 3.5 协商消息扩展（18779）

```rust
pub struct ShareRequest {                    // 向后兼容：endpointId 为空 = 旧语义
    pub device_id: String,
    pub device_name: String,
    #[serde(default)]
    pub endpoint_id: Option<String>,         // 新增：订阅目标端点
    #[serde(default)]
    pub delivery_mode: Option<Delivery>,     // 新增：订阅方期望方向
    #[serde(default)]
    pub relay_addr: Option<String>,          // 新增：push 模式下订阅方自己的中继 HTTP 基址（ws://ip:port）
    #[serde(default)]
    pub share_token: Option<String>,         // 新增：push 模式下订阅方**自签**的一次性接入凭证
                                             //   （订阅方先建会话+签凭证，公开方凭此出站推入订阅方中继）
    pub media: Vec<MediaKind>,               // 保留（旧端用；新端与端点 device.kind 一致）
}

pub struct ShareGrant {                      // 现有字段不变，新增：
    #[serde(flatten)]
    pub view: ShareTokenView,
    pub trusted: bool,
    #[serde(default)]
    pub delivery: Option<Delivery>,          // 公开方拍板后的方向（与请求可不同）
    #[serde(default)]
    pub transports: Option<Vec<TransportId>>,// pull 模式：公开方接受的传输（按优先序）
    #[serde(default)]
    pub relay: Option<RelayAddr>,            // pull 模式：公开方中继地址（ws/srt/quic 端口）
    // push 模式不开 relay：公开方凭 share_token 连订阅方 relay_addr
}
```

### 3.6 文件端点与传输协议（第三轮，Lossless）

文件端点（`MediaKind::File`）在公开时登记**本地文件源**（路径只存本地，绝不进
wire 目录/摘要）；订阅达成后公开方把文件当**数据流**推送，复用既有数据面
（watch/push/Hello，**中继零改动**）：

```rust
// stross-proto：文件元数据（首帧载荷，JSON）
pub struct FileMeta { pub name: String, pub size: u64, pub sha256: Option<String> }

// stross-proto frame：新增轨道与编解码值（u8 空间内扩展，旧端忽略未知轨道）
pub const TRACK_FILE: u8 = 2;    // 0=视频 1=音频 2=文件
pub const CODEC_FILE: u8 = 3;    // 1=H.264 2=AAC 3=文件（无编解码语义，占位）
```

帧序列（全部 TRACK_FILE / CODEC_FILE，seq=0 无损路径）：
1. **首帧** `FLAG_CONFIG`：载荷 = `FileMeta` JSON（文件名/大小）；
2. **数据帧**：载荷 = ≤64KiB 文件块（pts_ms = 块序，无时间语义）；
3. **末帧** `FLAG_END`：载荷 = 末块（空文件则空载荷）。

中继行为不变：`forward` 只缓存视频关键帧、`handle_watch` 只门控视频轨——
文件轨逐帧直通、不补发，因此**订阅方必须先接到流才开始推**（见 §5 等待观看者）。

---

## 4. 目录 API（协商端口 18779，LAN 可达、CORS 已放行）

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/api/endpoints` | 本节点 `{ node, devices, endpoints }`。**可见性过滤**：Private 端点只对白名单节点的 ip/device_id 可见（P1 按请求方 ip 匹配粗略，device_id 鉴权后置） |
| POST | `/api/negotiator/request` | 订阅握手（§5）；无 `endpointId` 时行为与现状完全一致（旧端兼容） |

> 目录不挂 18778（控制面仅回环，D7 门控不动）；新增路由挂在协商 Router 下，
> 复用现有 `cors_layer`。

---

## 5. 订阅握手序列

```
订阅方 A ──GET /api/endpoints (若白名单需带 device_id)──▶ 公开方 B
push 意向：A 先本地建会话 + 自签一次性凭证（现有 issue_share_token 语义）
A ──POST /api/negotiator/request { deviceId, endpointId, deliveryMode,
                                   relayAddr, shareToken?, media }──▶ B
B ── 决策表 ──▶ A:
    Public           → 自动签发（trusted=false，不写信任清单）
    Confirm + 已信任 → 自动签发（trusted=true）
    Confirm + 未信任 → 挂起 60s 人工确认（复用 PendingRequest/negotiator-respond；可记住）
    Private + 白名单 → 自动签发 / Private + 非白名单 → 403
    endpointId 不存在/未公开 → 404
A 收 ShareGrant { delivery, transports, relay?, ShareToken }：
    pull → A 连 B 的 relay 地址 watch（token.stream_id + Hello）
    push → B 凭 **A 自签的 shareToken** 出站推入 A 中继（A 侧 watch 自己的中继接收）
```

> **push 凭证修正（第三轮）**：push 方向的数据面接入凭证必须由**订阅方**签发
> （凭证校验器挂在订阅方内核上），公开方签发的凭证在订阅方中继校验不过。
> 因此 push 模式下订阅方在请求里随 `relay_addr` 附带自签 `share_token`；
> 公开方出站 Hello 出示该凭证。LAN 可信模型下与「二维码贴凭证」等价风险
> （§9）。pull 模式无需凭证（watch 路径不鉴权），公开方推入**自己的**受控
> 中继（回环来源 + 内核预授权会话放行）。

错误码：400 参数非法 / 403 被拒或超时 / 404 端点不存在 / 408/504 人工确认超时
（沿用现状 `handle_request` 分支）。

**联动（第三轮接线）**：公开方在**授予成功后**触发订阅事件（`SubscribeCtx`：
订阅方 device_id、定稿 delivery、数据面 stream_id、push 模式的 relay_addr 与
share_token），上层驱动按端点类型自动开推：
* 文件端点 → 文件泵：凭 stream_id 推入对应中继（pull=自己的受控中继，
  push=订阅方中继），**先等 ≥1 个观看者接入**（轮询中继 `/api/streams`，
  超时 8s）再发文件帧——避免广播不补发导致订阅方丢文件头；
* 媒体端点 → 复用 `start_stream`：pull 推本机中继（可被多订阅者观看），
  push 带订阅方凭证出站（复用既有 B2 手机推 PC 路径）；
* 无订阅者时端点回 Idle（P1：单次订阅推送完成即结束，常驻推送后置）。

---

## 6. 内核 EndpointRegistry（stross-app）

新模块 `crates/stross-app/src/kernel/endpoint.rs`：

```rust
pub struct EndpointRegistry {
    devices: HashMap<String, DeviceInfo>,        // 节点设备表（静态枚举 + 文件动态设备）
    endpoints: HashMap<String, EndpointManifest>,// 已公开端点（1:1：device_id ↔ endpoint_id）
    file_sources: HashMap<String, FileSource>,   // 文件端点：端点 id → 本地文件源（路径不落 wire）
}
pub struct FileSource { path: PathBuf, name: String, size: u64 }
pub struct SubscribeCtx {                        // 订阅事件载荷（驱动开推的依据）
    subscriber: String,                          //   ​订阅方节点 device_id
    delivery: Delivery,                          //   定稿方向
    stream_id: String,                           //   pull=公开方本机会话 / push=订阅方会话
    relay_addr: Option<String>,                  //   push：订阅方中继 HTTP 基址
    share_token: Option<String>,                 //   push：订阅方自签凭证
}
impl EndpointRegistry {
    pub fn publish(&mut self, device_id, visibility, delivery, transports) -> Result<EndpointManifest>;
    //  1:1 约束：同 device 已公开 → Err("该设备已公开")
    pub fn publish_file(&mut self, path, visibility, delivery) -> Result<EndpointManifest>;
    //  文件 = 动态设备（device_id "file:<名>"，重名自动加序号）；登记 file_sources
    pub fn unpublish(&mut self, endpoint_id) -> Result<()>;
    //  同时移除 file_sources
    pub fn manifest(&self, endpoint_id) -> Option<&EndpointManifest>;   // 供 /api/endpoints
    pub fn file_source(&self, endpoint_id) -> Option<&FileSource>;
    pub fn set_state(&mut self, endpoint_id, state, subscribers);
    pub fn on_subscribed(&self, endpoint_id, ctx: &SubscribeCtx);       // 触发驱动开推
}
```

- 设备表 P1 按平台静态枚举：桌面 `[screen:0, mic:builtin, sysaudio:builtin]`，
  Android `[mic:builtin, sysaudio:builtin, screen:0]`；`camera` 按现有采集能力
  决定是否枚举（开放问题 §11）。
- 状态：`subscribers` 复用关联会话的 `watchers`/`sinks` 计数，不另起炉灶。

---

## 7. 主程序代持与互斥

- **目录持有者 = 节点上的主程序**：桌面 serve 与 GUI 都代持（谁活着谁持有），
  **互斥启动**：复用身份目录，新增独占锁文件 `endpoint-registry.lock`
  （`create_new` 抢锁，异常退出由 OS 释放）；Android = GUI 代持。
- 其它进程（stross-relay、CLI relay）P1 **不代持也不注册**——通过本机回环问
  主程序（P1 后补，见 §8）；现状各进程各自广播行为维持到目录合并落地。

---

## 8. 兼容与演进

| 版本 | 兼容规则 |
|---|---|
| DiscoveryInfo v1 | v2 广播新增 `devices` 字段；v1 解析器忽略未知字段（serde 默认），不再带版本化的旧结构 |
| negotiator request v1 | 新字段全部 optional；旧端（无 endpointId）行为与现状逐字节一致 |
| ShareGrant v1 | 新增字段 optional；旧 UI 只读现有字段不受影响 |
| 数据面 | watch/push/Hello/ShareToken **零改动**，本框架只改发现与信令 |

P1 后扩展点：一设备多端点（endpoint_id 与 device_id 解耦）、文件/剪贴板端点
（二期 E，Lossless）、参数协商（分辨率/码率，复用 Offer/Answer 语义）、服务端点
（QUIC 多路复用 + schema）、其它进程回环注册目录。

---

## 9. 安全

- 可见性三档决定**目录可见性 + 授予决策**两件事：Private 端点不出现在
  `/api/endpoints` 响应（非白名单），mDNS 摘要只含 `published` 布尔、不含可见性；
- 信任按**节点**（`trusted_devices.json` 现有语义）：信任手机=手机上的设备端点免确认；
- 凭证复用 `ShareToken`（一次性、短时效、服务端比对），不进日志不进 mDNS TXT；
- 控制面 18778 仍仅回环；目录/订阅只走 18777/18779；LAN 可信模型不变。

---

## 10. P1 验收清单

1. 单测：`DiscoveryInfo v2` roundtrip/容错；`EndpointRegistry` publish/unpublish/
   1:1 约束；协商扩展解析（旧请求无 endpointId 兼容）；
2. 双机真机：A 订阅 B 屏幕端点（pull）观看闭环；A 订阅 B 麦克风端点（push）
   收声闭环（复用 `share-token-test.sh` 扩展）；
3. 可见性：Public 免确认 / Confirm 首见弹窗可记住 / Private 非白名单 403；
4. GUI：节点卡片→设备→公开（选可见性/协议/delivery）→ 状态与订阅数展示；
5. 兼容：旧端（无端点字段）发现/推流/观看全链路不受影响。

---

## 12. 实现记录

### P1（已提交 2f22f21）

| 落点 | 内容 |
|---|---|
| `crates/stross-proto/src/message/endpoint.rs` | DeviceInfo / DeviceSummary / Visibility / Delivery / TransportPreference / EndpointState / EndpointManifest + wire 单测 |
| `crates/stross-proto/src/message/ids.rs` | MediaKind 追加 `File` / `Service` 占位（顺带补齐 `apps/stross-cli/src/devices.rs` 的穷尽 match） |
| `crates/stross-proto/src/message/discovery.rs` | DiscoveryInfo v2：`devices` 摘要 + `VERSION=2` + `with_devices` + 兼容单测（v1 载荷解析） |
| `crates/stross-app/src/kernel/endpoint.rs` | EndpointRegistry：publish/unpublish（1:1 约束）、状态、订阅 hook、默认传输 + 单测 |
| `crates/stross-app/src/kernel/graph.rs` | 既有 `Endpoint` 重命名为 `TransportAddr`（避免与端点概念撞名） |
| `crates/stross-app/src/app.rs` | registry 字段、平台设备静态枚举、发布/查询方法、mDNS 摘要接入 |
| `crates/stross-app/src/negotiator.rs` | ShareRequest/ShareGrant/PendingRequest 扩展、`policy_decision`、`compose_grant`、`GET /api/endpoints` |
| `crates/stross-app/src/lib.rs` | 导出 EndpointRegistry / RelayAddr / TransportAddr |

### 第二轮：引导层 + L1 浏览闭环（未提交）

| 落点 | 内容 |
|---|---|
| `crates/stross-app/src/bootstrap.rs` | 引导层编排门面：`ensure_identity` / `anchor`（中继锚定 + mDNS L1）/ `start_handshake`（18779 目录+握手）/ `start` 完整组合；CLI serve 与 GUI 桌面启动均接入 |
| `crates/stross-app/src/app.rs` | `RelayInfo.devices`（L1 设备摘要：本机 = 注册表快照，对端 = mDNS 解码）；`scan_relays` 透传 |
| `apps/stross-cli/src/devices.rs` | `stross devices` 输出每节点设备清单（含「已公开」标记） |
| `crates/stross-core/src/discovery.rs` | 选址改纯函数 `select_reachable_ip_from(self_ips, reachable)`——测试显式注入本机网段，与环境解耦（原硬编码"本机在某网段"，网段迁移后必挂）；`BrowseAgg` 类型别称修 clippy type-complexity |

未做（后续步骤）：GUI 前端渲染设备清单 / 公开与订阅交互、目录互斥锁、
设备重命名、`/api/endpoints` 的 Private 白名单动态过滤（当前一律不下发
Private 端点）、mDNS L1 摘要在 publish/unpublish 时重广播（当前仅锚定时刻）。

### 第三轮：文件端点 + 订阅联动 + 端点命令（已完成待提交）

| 落点 | 内容 |
|---|---|
| `stross-proto` | `TRACK_FILE`=2 / `CODEC_FILE`=3（frame.rs）；`FileMeta`（endpoint.rs，首帧 CONFIG 载荷）；`ShareRequest.share_token`（push 凭证修正 §5）；**DiscoveryInfo TXT 拆多 key**（§3.4 方案 b：`stross` + `dev.<n>`，实测 449B 超限 → 每 key ≤255B，v1 双向兼容） |
| `stross-app` | EndpointRegistry `publish_file` / `file_source` / `SubscribeCtx` / `subscribed_hook`（hook 克隆出锁调用，**修复持锁回调死锁**——曾挂死订阅握手）；`file_xfer.rs` 文件泵+文件接收（**等观看者接入再推**，轮询 `/api/streams`，8s 超时；空文件 END 帧路径）；`endpoint_driver.rs` 订阅联动（文件→文件泵，屏幕/麦克风→start_stream） |
| `stross-app` 协商 | PendingEntry 携带 relay_addr/share_token；授予成功后 `notify_subscribed` → `SubscribeCtx` 触发驱动（pull 用公开方会话，push 用订阅方凭证内的流 id） |
| `stross-core` | `RelayClient` shutdown 前**冲完帧通道排队帧**（修复 stop 抢占丢失文件末帧，`try_recv` 排空后 Bye） |
| `stross-app` 控制面 | CtrlRequest `EndpointPublish` / `EndpointPublishFile` / `EndpointUnpublish` / `EndpointList` |
| `stross-cli` | serve `--negotiator-port` / `--data-dir`（本地双端不同身份/端口）；ctrl `endpoint publish/publish-file/unpublish/list`；新增 `endpoint ls`（L2 目录拉取）与 `endpoint subscribe`（pull/push 双向收文件；**watch 对「流尚未出现」重试**收敛建流竞态；push 模式**watch 自己签发的流**——曾误用 grant 会话 id） |
| 验证 | 单测（FileMeta / Registry 文件源 / 订阅 ctx / DiscoveryInfo 多 key ≤255B 回归）；进程内中继文件泵↔文件收（含空文件）集成测试；**本地双端脚本 `scripts/dual-node-file-test.sh`：3 条链路（A→B pull 800KB / A→B push / B→A pull）文件逐字节一致** |

本地双端实测（2026-08-27，同机双 serve）：
`stross devices` 同时发现两节点（L1 摘要闭环，0 次 TXT 超限）；
`endpoint ls` 拉到动态文件设备与可订阅端点（L2）；`endpoint subscribe`
404 边界（端点不存在 / 已取消公开）均正确拒绝。

---

## 11. 开放问题（实施前实测/拍板）

1. ~~**TXT 255B 上限**~~：**已解决（第三轮）**——实测整包 449B 超限，拍板 §3.4
   方案 b（拆多 key），见 §3.4 与 §12 第三轮记录；
2. **camera 设备是否枚举**：按现有采集能力（Android 摄像头/桌面 WebCam）决定 P1 是否列出；
3. **pull 模式"订阅→公开方自动建会话推流"真机验证**（B2 自动接收反向复用）；
4. **设备重命名**：P1 用默认名，改名（本地持久化）是否入 P1。