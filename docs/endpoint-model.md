# 端点框架 · 设计规格（节点 → 端点，单层模型 + load/share 契约）

> 状态：**讨论定稿；第六轮已提交**（单层端点模型——设备/端点合并为一层，
> 端点自维护可挂载性 + load/share 行为契约，屏幕获取失败前置化为 load 探测）。
> 待办：目录互斥锁、真机回归、Private 白名单动态过滤、publish 时 mDNS 重广播、
> 运行期重探测（权限授予后 reload）。对齐 `iteration-plan.md` 阶段 A/B。
> 本文件是协议与实现的唯一规格源；术语定稿见 §1，任何讨论改动须先改这里。

---

## 1. 术语（定稿）

| 层级 | 术语 | 英文 | 定义 |
|---|---|---|---|
| L0 | **节点** | Node | 物理机器上的一个 Stross 运行实例（手机、电脑）；**mDNS 广播单位**，有 IP、有持久身份（`identity.json` 的 device_id） |
| L1 | **端点** | Endpoint | 节点上**可共享的能力实体**：屏幕、麦克风、摄像头、系统声音、文件、服务……与订阅与否无关，一直存在，有稳定 id。**自维护「可挂载性」**（`available`：能否被挂载成节点） |
| — | 行为契约 | load / share | 端点 ↔ 内核的**约定**（非语言特性）：每个端点实现 `load`（探测自身可用性 → 维护 available/last_error）与 `share`（订阅达成后启动共享推流）。内核不做类型分派 |
| — | 目标类型 | TargetKind | 端点分两类：**确定目标**（文件等，内容预先确定，一次推送、有完成态）与**实时目标**（相机等，内容持续产生，持续推流）。共性 = load/share 契约；差异 = 目标类型 + 各端点实现 |
| — | 通告 / 订阅 / 推流 | Publish / Subscribe / Push | 通告=端点参数化（可见性/delivery/协议）并进入对端目录；订阅=对端点的意图登记→握手→凭证→数据面；推流=订阅成立后的传输，沿用 watch/push 双路径 |
| — | 引导层 | Bootstrap | 发现（节点级 mDNS）+ 目录（节点→端点）+ 订阅握手；不是独立进程，是每节点自带的逻辑层 |

协议 wire 字段统一英文 `nodeId / endpointId`（与现有 `stream_id` 风格一致）；
本表中文术语只管 UI 与文档。UI：原"设备卡片"改称**节点卡片**，卡片内列**端点**。

---

## 2. 单层模型与数据流

```
节点 A（手机）                    节点 B（电脑）
  ├─ 端点 mic:builtin   ◄──通告──► 对端目录（B 可见、可订阅）
  ├─ 端点 screen:0                 目录持有者 = 节点（每节点一个）
  └─ 端点 sysaudio:builtin
```

**Pull 流（订阅者连公开者）** —— 订阅 B 的屏幕：
B 通告「屏幕」端点（idle，无订阅）→ A 发现节点 B → 拉端点详情 → 订阅
（Public/Confirm/Private 决策）→ 授予接入 → B 收到订阅事件**自动调端点
`share`**（端点自驱动：建会话并启动采集/推流）→ A 连 B 中继 watch（现有
`Watch` 路径零改动）。

**Push 流（公开者连订阅者）** —— 订阅 B 的麦克风（反向外设）：
B 通告「麦克风」端点 → A 订阅时在请求里附带**自己的中继地址**（`/api/info`
的 ws/srt/quic 端口）→ 授予凭证（现有 `ShareToken`）→ A 侧中继等待 →
B 凭凭证出站 push 到 A 中继（现有 `Hello + share_token` 路径零改动）。

**挂载生命周期**：端点 `seed` 时立即 `load`（探测可用性）→ `available` +
`last_error`。**不可挂载端点保留在表里**（UI 显示原因），但不可通告、不可
订阅。**采集生命周期解耦**：端点可常驻 Idle；订阅达成才建会话、启动采集/
编码；无订阅者不采集（省电/省资源）。

---

## 3. 数据模型（stross-proto，wire v3）

> **v3 为破坏性升级**（单层端点模型）：`DeviceInfo` / `DeviceSummary` 消失，
> `EndpointManifest` 平铺、`DiscoveryInfo.endpoints` 携带 `available`、
> 目录 `EndpointDir` 不再有独立 `devices` 数组。旧端（v2 及以前）与新端
> 互不解析对方目录/摘要，**需全端同步升级**。新增字段带 `#[serde(default)]`
> / `skip_serializing_if` 的惯例保留。

### 3.1 端点种类

`MediaKind` 沿用：`Screen, Window, Camera, Mic, SystemAudio, Input, Clipboard,
File, Service`（`Input` / `Service` 为游戏联机等扩展预留，见 §13）。

### 3.2 端点摘要与清单

```rust
/// mDNS 摘要层（L1，DiscoveryInfo v3）用——只带 id/kind/name/可挂载/已通告。
pub struct EndpointSummary {
    pub endpoint_id: String,
    pub kind: MediaKind,
    pub name: String,
    pub available: bool,     // load 探测结果：能否被挂载成节点
    pub published: bool,
}

/// 端点清单（目录 L2 / 本机目录 / 控制面）：公开方协议/可见性/挂载性的唯一来源。
pub struct EndpointManifest {
    pub endpoint_id: String,
    pub kind: MediaKind,
    pub name: String,
    pub available: bool,                 // load 探测结果
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,      // load/share 失败原因（不可用时展示）
    pub published: bool,                 // 是否已通告（未通告仅本机可见）
    pub visibility: Visibility,
    pub delivery: Delivery,
    pub transports: Vec<TransportPreference>,
    pub codecs: Vec<CodecId>,
    pub state: EndpointState,            // idle | active | suspended
    pub subscribers: u32,
    pub updated_at: u64,
}
```

> 协议选择的正确性由目标类型兜底：确定目标 → Lossless（QUIC/WS）；
> 实时目标 → Lossy/Adaptive（SRT/QUIC/WebRTC）。公开者按端点目标类型选协议
> （`EndpointRegistry::default_transports(target)`），订阅者只在 `transports`
> 列表内协商/降级（复用 `Offer`/`Answer`）。

### 3.3 DiscoveryInfo v3（L1 摘要）

```rust
pub struct DiscoveryInfo {
    pub v: u8,                               // 3
    pub name: String,
    pub roles: Vec<RoleId>,
    pub media: Vec<MediaKind>,               // 端点能力总和，兼容旧端
    pub transports: Vec<TransportId>,
    pub codecs: Vec<CodecId>,
    #[serde(default)]
    pub endpoints: Vec<EndpointSummary>,     // 新增（v3）：端点清单摘要
}
```

**TXT 上限（§11.1 方案 b 沿用）**：mDNS TXT 单条 character-string ≤ 255B。
`stross` key 承载 base（恒不含 `endpoints`）；每个端点各占 `ep.<n>` key。

### 3.4 协商消息扩展（18779，不变）

`ShareRequest` / `ShareGrant` 与 v2 逐字节一致（`endpointId` 非空 = 订阅端点）。

### 3.5 文件端点与传输协议（沿用第三轮）

`TRACK_FILE=2` / `CODEC_FILE=3` / `FileMeta` 首帧载荷 / 帧序列（CONFIG → 数据
块 → END）不变；中继零改动。文件 = 确定目标端点的典型实现。

---

## 4. 目录 API（协商端口 18779，LAN 可达、CORS 已放行）

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/api/endpoints` | 本节点 `{ node, endpoints }`：**已通告**端点清单。不可挂载端点（`available=false`）**可见但不可订阅**（UI 灰显 + 原因；握手校验拒绝）。**Private 端点只对白名单节点的 ip/device_id 可见**（P1 按请求方 ip 匹配粗略，device_id 鉴权后置） |
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
    endpointId 不存在/未通告 → 404
    endpointId 不可挂载（available=false）→ 404 + 原因（last_error）
A 收 ShareGrant { delivery, transports, relay?, ShareToken }：
    pull → A 连 B 的 relay 地址 watch（token.stream_id + Hello）
    push → B 凭 **A 自签的 shareToken** 出站推入 A 中继（A 侧 watch 自己的中继接收）
```

> **push 凭证修正（第三轮，沿用）**：push 方向的数据面接入凭证必须由**订阅方**
> 签发；公开方签发的凭证在订阅方中继校验不过。LAN 可信模型下与「二维码贴
> 凭证」等价风险。pull 模式无需凭证（watch 路径不鉴权），公开方推入**自己的**
> 受控中继（回环来源 + 内核预授权会话放行）。

错误码：400 参数非法 / 403 被拒或超时 / 404 端点不存在或不可挂载 /
408/504 人工确认超时。

**联动（契约化接线）**：公开方在**授予成功后**触发订阅事件（`SubscribeCtx`：
订阅方 device_id、定稿 delivery、数据面 stream_id、push 模式的 relay_addr 与
share_token），**端点自驱动**——协商层直接调 `endpoint.share(app, ctx)`，
内核不再按类型分派（原 `endpoint_driver` 的 match 分派已删除）：
* 文件端点（确定目标）→ 文件泵：凭 stream_id 推入对应中继（pull=自己的
  受控中继，push=订阅方中继），**先等 ≥1 个观看者接入**（轮询中继
  `/api/streams`，超时 8s）再发文件帧——避免广播不补发导致订阅方丢文件头；
  传完自动回 Idle（有完成态）；
* 媒体端点（实时目标）→ 各端点自行组 `StreamConfig` 调 `start_stream`：
  pull 推本机中继（可被多订阅者观看），push 带订阅方凭证出站；持续推流
  直到停止；
* 新增端点类型（剪贴板/服务/游戏输入）→ 新 struct 实现 load/share，
  **内核与协商层零改动**。

---

## 6. 内核 EndpointRegistry（stross-kernel，单层表）

模块 `crates/stross-kernel/src/kernel/endpoint.rs`：

```rust
/// 端点 ↔ 内核行为契约（§1）：每个端点实现两个约定行为——
/// load：探测自身可用性（能否被挂载成节点），维护 available/last_error；
/// share：订阅达成后启动共享（推流），类型自决，内核不做分派。
pub trait Endpoint: Send + Sync {
    fn id(&self) -> &str;
    fn kind(&self) -> MediaKind;
    fn name(&self) -> &str;
    fn target(&self) -> TargetKind;          // Determined | Live（§1 目标类型）
    fn available(&self) -> bool;             // load 探测结果
    fn last_error(&self) -> Option<&str>;
    fn load(&mut self) -> std::result::Result<(), String>;
    fn share(&self, app: Arc<Kernel>, ctx: SubscribeCtx);
}

/// load 探测函数：平台适应层注入（查环境/设备/权限），内核零 OS 调用。
pub type Probe = Arc<dyn Fn() -> std::result::Result<(), String> + Send + Sync>;

pub struct EndpointRegistry {
    endpoints: HashMap<String, EndpointEntry>,   // 单表：行为对象 + 通告参数
    file_sources: HashMap<String, FileSource>,   // 文件端点本地源（不落 wire）
}
pub struct EndpointEntry {
    ep: Arc<dyn Endpoint>,
    published: bool, visibility: Visibility, delivery: Delivery,
    transports: Vec<TransportPreference>, codecs: Vec<CodecId>,
    state: EndpointState, subscribers: u32, updated_at: u64,
}
impl EndpointRegistry {
    pub fn seed(&mut self, ep: Box<dyn Endpoint>) -> bool;
    //  登记端点并立即 load；id 已存在返回 false。load 失败不阻止登记：
    //  端点保留但 available=false + last_error（UI 可见原因）
    pub fn publish(&mut self, id, visibility, delivery, transports, codecs) -> Result<EndpointManifest>;
    //  不可挂载（available=false）拒绝，错误携带原因；重复通告报错
    pub fn unpublish(&mut self, endpoint_id) -> Result<()>;   // 端点保留，可再次通告
    pub fn publish_file(&mut self, path, visibility, delivery) -> Result<EndpointManifest>;
    //  文件 = 动态端点（endpoint_id "file:<名>"，重名自动加序号）
    pub fn manifest / manifests / published_manifests / summaries(...);
    pub fn on_subscribed(&self, app: &Arc<Kernel>, endpoint_id, ctx);
    //  出锁克隆端点对象后调 share（端点自驱动；持锁调用会死锁）
    pub fn default_transports(target: TargetKind) -> Vec<TransportPreference>;
    //  实时目标 → QUIC>SRT>WS；确定目标 → QUIC>WS（不再按 MediaKind 匹配）
}
```

- 端点实现：`ScreenEndpoint` / `MicEndpoint` / `SystemAudioEndpoint`
  （实时目标，探测闭包注入）+ `FileEndpoint`（确定目标，动态构造）；
  构造器在 stross-bridge（平台筛选 + 探测闭包），kernel 只收行为对象；
- 目标类型维度表达差异：默认传输（Lossless/Lossy）、共享生命周期
  （文件传完回 Idle / 媒体持续推流）；
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
| wire v2 → v3 | **破坏性升级**：`DeviceInfo`/`DeviceSummary` 消失，`EndpointManifest` 平铺（kind/name/available/lastError/published），`DiscoveryInfo.endpoints`、`EndpointDir` 去 devices。旧端忽略未知字段惯例不再覆盖整体结构变化，需全端同步升级 |
| negotiator request v1 | 字段全部 optional；旧端（无 endpointId）行为与现状逐字节一致（v3 未动） |
| ShareGrant v1 | 未动 |
| 数据面 | watch/push/Hello/ShareToken **零改动**，本框架只改发现与信令 |

P1 后扩展点：运行期重探测（权限授予后 reload）、一设备多端点（endpoint_id
与 kind 解耦）、参数协商（分辨率/码率，复用 Offer/Answer 语义）、服务端点
（QUIC 多路复用 + schema）、其它进程回环注册目录。

---

## 9. 安全

- 可见性三档决定**目录可见性 + 授予决策**两件事：Private 端点不出现在
  `/api/endpoints` 响应（非白名单），mDNS 摘要只含 `published` 布尔、不含可见性；
- 信任按**节点**（`trusted_devices.json` 现有语义）：信任手机=手机上的端点免确认；
- 凭证复用 `ShareToken`（一次性、短时效、服务端比对），不进日志不进 mDNS TXT；
- 控制面 18778 仍仅回环；目录/订阅只走 18777/18779；LAN 可信模型不变。

---

## 10. P1 验收清单

1. 单测：`DiscoveryInfo v3` roundtrip/容错；`EndpointRegistry` seed/publish/
   unpublish/不可挂载拒绝；load 契约（探测失败 → available=false + 原因）；
2. 双机真机：A 订阅 B 屏幕端点（pull）观看闭环；A 订阅 B 麦克风端点（push）
   收声闭环（复用 `share-token-test.sh` 扩展）；
3. 可见性：Public 免确认 / Confirm 首见弹窗可记住 / Private 非白名单 403；
4. GUI：节点卡片→端点（通告选可见性/协议/delivery）→ 状态与订阅数展示；
   不可挂载端点灰显 + 原因（屏幕获取失败可见化）；
5. 兼容：旧语义（无 endpointId）发现/推流/观看全链路不受影响。

---

## 11. 开放问题（实施前实测/拍板）

1. ~~**TXT 255B 上限**~~：**已解决（第三轮）**——方案 b（拆多 key），v3 沿用；
2. **camera 端点是否构造**：按现有采集能力（Android 摄像头/桌面 WebCam）决定 P1 是否列出；
3. **pull 模式"订阅→公开方自动建会话推流"真机验证**（B2 自动接收反向复用）；
4. **运行期重探测**：权限授予（Android 麦克风）后如何触发端点 reload
   （当前 load 仅 seed 时一次）。

---

## 12. 实现记录

### 第六轮：单层端点模型 + load/share 契约（本次提交）

问题：设备/端点两层 wire 身份 + `endpoint_driver` 按 `MediaKind` match 分派 +
`platform_devices` 静态枚举——新增端点类型要改内核；「屏幕获取失败」要到
订阅后推流才炸（headless/无 DISPLAY 时设备表照样有 screen:0）。

| 落点 | 内容 |
|---|---|
| `stross-proto` | `DeviceInfo`/`DeviceSummary` 删除；`EndpointManifest` 平铺（kind/name/available/lastError/published）；新增 `EndpointSummary`（available/published）；`DiscoveryInfo v3.endpoints`（TXT key `ep.<n>`）；`EndpointDir` 去 devices |
| `stross-kernel` | `kernel/endpoint.rs`：`Endpoint` trait（load/share 契约 + `TargetKind`）+ `ScreenEndpoint`/`MicEndpoint`/`SystemAudioEndpoint`/`FileEndpoint` + 注册表单表化（`seed` 即 load，不可挂载保留表内）；`endpoint_driver.rs` **删除**（端点自驱动，协商层直接调 share）；`default_transports` 改按 `TargetKind`；Kernel：`seed_device→seed_endpoint`、`on_endpoint_subscribed(app, ...)`、`endpoint_catalog→Vec<EndpointManifest>`；negotiator：目录只出已通告端点、不可挂载握手 404+原因；view/control/devices 摘要同步 |
| `stross-bridge` | `platform_endpoints` + `seed_platform_endpoints`：构造端点 + **load 探测闭包注入**（`screen_probe`：ffmpeg + DISPLAY/WAYLAND——屏幕获取失败前置化；音频探测：ffmpeg）；依赖新增 `stross-media` |
| `stross-cli` | `endpoint ls` 输出端点清单（可用/不可用+原因）；`ctrl endpoint list` 单表输出（available/lastError/published）；`devices` 端点摘要含不可用标记 |
| 顺带修复 | 控制面客户端 `request` 匹配 `rsp:"error"` 而 serde 实际序列化 `"err"`（变体名小写）——错误响应被当事件忽略导致客户端无限等待（**既有 bug**，首个常用错误响应路径暴露） |
| `stross-gui` | 前端端点树（available 灰显 + lastError；对端目录不可订阅灰显）；`state.ts`/`endpoints.ts`/`discovery.ts` 单层类型 |
| 验证 | proto wire 单测（平铺 manifest/EndpointSummary/v3 摘要）；registry 单测（load 契约/不可挂载拒绝/单表 publish）；negotiator 订阅 share 契约测试；bridge 平台端点测试；前端无头 24 项断言全过；workspace 全测试过（`stross-media` 播放节奏测试为环境性波动，干净树同样失败） |

### 前几轮（P1 2f22f21 / 二轮 ab5dd9b / 三轮 2182bc0 / 四轮分层收敛 / 五轮 UI 收敛）

见 git 历史与 `docs/iteration-plan.md`；第三轮的文件端点/订阅联动/本地双端
脚本（`scripts/dual-node-file-test.sh`）链路在 v3 下保持逐字节一致。

---

## 13. 扩展性主张（游戏联机等场景）

端点框架的地基属性：**新增端点类型 = 新增一个实现 load/share 的 struct +
构造器（含探测闭包），内核与协商层零改动**。预留的 `MediaKind::Input`
（游戏手柄/键鼠注入，实时目标、延迟敏感）与 `MediaKind::Service`（游戏服务
托管，QUIC 多路复用 + schema）即为该方向的扩展口；确定目标（存档/Mod/资源
分发）与实时目标（观战/语音/输入）的分类已覆盖游戏联机的数据形态。
