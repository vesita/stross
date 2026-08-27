# 端点框架 · 设计规格（节点 → 设备 → 端点）

> 状态：**讨论定稿；P1 已提交（2f22f21）**。第二轮（未提交）：引导层 `bootstrap`
> 编排门面（CLI serve / GUI 桌面均接入）、浏览侧 L1 设备摘要消费
> （`scan_relays` / `stross devices` 输出）、选址测试改为注入式纯函数。
> 待办：CtrlServer 命令、订阅→推流联动、真机验证、目录互斥锁。
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

**TXT 上限风险（实施前必实测）**：mDNS TXT 单条 character-string ≤ 255B（RFC 1035），
公告走 UDP 组播（典型 MTU 内）。当前"单 key 大 JSON"已接近边界，加 `devices`
摘要必超。兜底方案（按实测结果选一）：
a) **摘要精简**：`devices` 只留 `{id, kind}`（name 与 published 进 L2），单设备 ≈ 40B；
b) **拆多 key**：每设备一条 TXT key（`dev:mic:builtin`），放弃"单 key 零维护"便利；
c) **不进摘要**：TXT 只加 `deviceCount`，设备清单全部 L2 拉取（最保守，首屏少一层信息）。

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
    pub relay_addr: Option<String>,          // 新增：push 模式下订阅方自己的 /api/info 地址
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
A ──POST /api/negotiator/request { deviceId, endpointId, deliveryMode, relayAddr, media }──▶ B
B ── 决策表 ──▶ A:
    Public           → 自动签发（trusted=false，不写信任清单）
    Confirm + 已信任 → 自动签发（trusted=true）
    Confirm + 未信任 → 挂起 60s 人工确认（复用 PendingRequest/negotiator-respond；可记住）
    Private + 白名单 → 自动签发 / Private + 非白名单 → 403
    endpointId 不存在/未公开 → 404
A 收 ShareGrant { delivery, transports, relay?, ShareToken }：
    pull → A 连 B 的 relay 地址 watch（token.stream_id + Hello）
    push → A 侧中继就绪 → B 凭 token 出站 push（Hello + share_token）
```

错误码：400 参数非法 / 403 被拒或超时 / 404 端点不存在 / 408/504 人工确认超时
（沿用现状 `handle_request` 分支）。

**联动**：pull 模式授予后，B 内核收到订阅事件 → 为该端点**自动建会话并启动推流**
（复用 B2 电脑端"自动接收"同款逻辑，方向对调）；无订阅者时端点回 Idle。

---

## 6. 内核 EndpointRegistry（stross-app）

新模块 `crates/stross-app/src/kernel/endpoint.rs`：

```rust
pub struct EndpointRegistry {
    devices: HashMap<String, DeviceInfo>,        // 节点设备表（P1 静态枚举）
    endpoints: HashMap<String, EndpointManifest>,// 已公开端点（1:1：device_id ↔ endpoint_id）
    // 订阅事件：Tauri/CLI 侧注册回调，pull 模式授予时触发"建会话+推流"
}
impl EndpointRegistry {
    pub fn publish(&mut self, device_id, visibility, delivery, transports) -> Result<EndpointManifest>;
    //  1:1 约束：同 device 已公开 → Err("该设备已公开")
    pub fn unpublish(&mut self, endpoint_id) -> Result<()>;
    //  宽限：已订阅会话允许继续 3 分钟（参考 PUSH_SILENCE_TIMEOUT 语义），之后断开
    pub fn manifest(&self, endpoint_id) -> Option<&EndpointManifest>;   // 供 /api/endpoints
    pub fn set_state(&mut self, endpoint_id, state, subscribers);
    pub fn on_subscribed(&mut self, endpoint_id) -> Result<()>;         // 触发采集/推流联动
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

未做（后续步骤）：CtrlServer 命令（endpoint publish/unpublish）、GUI 前端渲染设备清单、
订阅达成自动建会话推流（`set_subscribe_hook` 已留口）、目录互斥锁、设备重命名、
`/api/endpoints` 的 Private 白名单动态过滤（当前一律不下发 Private 端点）。

---

## 11. 开放问题（实施前实测/拍板）

1. **TXT 255B 上限**：实测现网 `DiscoveryInfo` 尺寸 + devices 摘要，定 §3.4 的兜底方案；
2. **camera 设备是否枚举**：按现有采集能力（Android 摄像头/桌面 WebCam）决定 P1 是否列出；
3. **pull 模式"订阅→公开方自动建会话推流"真机验证**（B2 自动接收反向复用）；
4. **设备重命名**：P1 用默认名，改名（本地持久化）是否入 P1。