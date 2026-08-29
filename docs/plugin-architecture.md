# Stross 插件化架构设计（内核 + 可插拔传输）

> 状态：**阶段 2 完成，四传输落地**（阶段 1：transport-webrtc（str0m，datachannel
> 双通道）、relay WebRTC 信令端点、同一 handle_watch 驱动 ws/webrtc 集成测试通过、
> 能力协商落地、接收端 WebRTC 路径带回退）
> · 阶段 2 已落地：拆 `stross-transport` crate、首个 Sink（录制 RecordingSink）、
> 控制面鉴权（`AuthPolicy`/`PinAuthPolicy`）、`transport-srt`（rsrt 纯 Rust，
> Adaptive）、`transport-quic`（quinn + rustls-ring，control/media 多路复用）——
> 四种传输共用同一 `handle_push`/`handle_watch`（抽象价值四重证明）
> · 决策推迟：WASM 策略插件、跨设备控制闭环、Sink 其余（见 §5.3 决策记录）
> 关联：[architecture.md](architecture.md)（分层架构）· [protocol.md](protocol.md)（线上协议）· [roadmap.md](roadmap.md)（P0 设备路由 / P2 流解耦 / WebRTC 低延迟）

## 1. 背景与目标

Stross 现有分层架构（proto → transport → kernel → bridge → UI）已经是一个「内核」：
`stross-proto` 是传输无关的线上契约，`stross-endpoint::capture::CaptureBackend` 已经是第一个插件接口，
`stross-kernel::relay` 是数据面。设计之初它还缺三件事：

1. **控制面（内核）与数据面耦合**：会话、路由、方向控制没有独立抽象，`RelayServer` 的 Router 同时干两件事；
2. **传输层是 WS 专属**：帧格式注释写明「每个 WebSocket 二进制消息是一个完整的帧」，换传输要动协议；
3. **只有 Source 侧能力**：没有 Sink 侧（渲染 / 注入），也没有能力广播与协商。

本设计的目标：

- **内核（控制面）**：设备图、会话拓扑、路由（传输方向控制）、能力协商、鉴权，与编解码、传输完全解耦；
- **传输层可插拔**：无损（TCP-like）/ 有损（UDP-like）/ 自适应三种可靠性契约，WS 为第一个实现，
  WebRTC / QUIC / SRT 后续按需接入；
- **能力插件化**：Source（采集）与 Sink（渲染/注入/剪贴板）统一为能力注册表，
  为远期「Deskflow 类」键鼠注入、剪贴板共享留出同一条会话/路由/传输基座；
- **UI 保持薄**：桌面 / Android / 接收端都是内核命令 + 事件的消费方。

这与 roadmap 的 P0（设备路由，类似投屏）、P2（流解耦 / 数据面控制面分离）、
WebRTC 低延迟通道是同一个方向的统一抽象。

## 2. 目标架构总览

```
┌──────────────────────────────────────────────────────────────────────┐
│ UI 层（薄消费方）                                                      │
│   apps/stross-gui（Tauri 壳）/（远期）独立接收 App（stross-viewer） │
│   只调内核命令 + 订阅内核事件，不直接碰传输与采集                        │
├──────────────────────────────────────────────────────────────────────┤
│ 内核（控制面）stross-kernel                                            │
│   DeviceGraph（设备图）                                                │
│   CapabilityRegistry（能力注册表：Source / Sink）                       │
│   SessionManager（会话拓扑：source → sinks[]，含协商结果）              │
│   Router（路由：直连 / 经中继 / 组播路径选择，传输方向控制）              │
│   Auth（会话级访问码，控制面强制）                                      │
├──────────────────────────────────────────────────────────────────────┤
│ 能力插件（编译期 trait 注册，feature-gated）                            │
│   Source：CaptureBackend 演进（屏幕/窗口/摄像头/麦克风/系统声）          │
│   Sink：渲染 / 录制 /（远期）键鼠注入 / 剪贴板                          │
├──────────────────────────────────────────────────────────────────────┤
│ 传输插件（编译期 trait 注册，feature-gated）                            │
│   transport-ws（现状）→ transport-webrtc（有损低延迟）                  │
│   → transport-quic（无损多路复用）→ transport-srt（自适应）             │
├──────────────────────────────────────────────────────────────────────┤
│ stross-proto：传输无关帧（v2 头含 seq/分片）+ 控制消息（含协商/路由）     │
└──────────────────────────────────────────────────────────────────────┘
```

数据面只做一件事：把 `Frame` 沿会话拓扑搬到目的地。现有 relay 的关键帧对齐、
掉帧重对齐逻辑原样保留，只是从「WS 专用」变成「任意 Transport 之上的转发」。

### 插件机制选型

| 层 | 机制 | 理由 |
|---|---|---|
| 传输 / 能力（热路径） | **编译期插件**：feature + trait 注册表（沿用 `discovery` feature 的现有玩法） | 类型安全、零开销、无 ABI 问题；LAN 工具不需要运行时装插件 |
| 控制面策略（远期可选） | **WASM 插件**（Extism / wasmtime）：鉴权策略、路由策略、自动化脚本 | 沙箱 + 可热加载，但只放控制面，不进媒体热路径 |
| 明确不做 | 动态库（libloading）、element/pad 级媒体流水线（GStreamer 式） | Rust ABI 脆弱；插件粒度停在「能力级」即可，流水线级过重 |

## 3. 内核设计

### 3.1 核心类型

```rust
/// 一个参与互联的节点（本机或局域网内的其它 Stross 设备）。
pub struct NodeInfo {
    pub node_id: NodeId,
    pub name: String,
    pub roles: Vec<NodeRole>,          // Sender | Viewer | Relay | Controller
    pub caps: Vec<CapabilityDescriptor>, // 能力广播（mDNS TXT / 协商消息）
    pub endpoints: Vec<Endpoint>,      // { transport, addr } 候选
}

/// 能力描述（Source 与 Sink 统一）。
pub struct CapabilityDescriptor {
    pub kind: CapabilityKind,          // Source | Sink
    pub media: Vec<MediaKind>,         // Screen | Window | Camera | Mic | SystemAudio | Input | Clipboard
    pub codecs: Vec<CodecId>,          // h264 / aac / opus / av1 ...
    pub transports: Vec<TransportId>,  // ws / webrtc / quic / srt
    pub max_resolution: Option<(u32, u32)>,
    pub preferred_profile: ReliabilityProfile, // 该能力期望的传输可靠性
}

/// 会话：一条「从 A 推送到 B（可多个）」的互联。
pub struct Session {
    pub id: SessionId,
    pub source: NodeId,
    pub sinks: Vec<NodeId>,
    pub path: RoutePath,               // 数据面当前路径
    pub negotiated: Negotiated,        // { transport, codec, profile }
}

/// 路由路径（「控制传输方向」的直接体现）。
pub enum RoutePath {
    Direct(NodeId),                    // 直连（能力允许时优先）
    ViaRelay(NodeId),                  // 经中继兜底
    Mesh(Vec<NodeId>),                 // 组播 / 多目标
}
```

### 3.2 内核门面 API（草图）

```rust
pub struct Kernel { /* DeviceGraph + CapabilityRegistry + SessionManager + Router + Auth */ }

impl Kernel {
    pub async fn register_capability(&self, node: NodeId, desc: CapabilityDescriptor) -> Result<()>;
    pub async fn discover(&self) -> Vec<NodeInfo>;                      // mDNS 聚合 + 能力合并
    pub async fn create_session(&self, src: NodeId, sinks: &[NodeId],
                                prefs: SessionPrefs) -> Result<SessionId>; // 含传输/编解码协商
    pub async fn route(&self, id: SessionId, path: RoutePath) -> Result<()>; // 改传输方向
    pub async fn teardown(&self, id: SessionId) -> Result<()>;
    pub fn events(&self) -> broadcast::Receiver<KernelEvent>;           // 推给 UI（替代轮询）
}
```

选路策略（Router 内部）：先尝试直连（双方 `endpoints` 里有同一传输且 profile 匹配）→
失败回退经中继；`route()` 可在会话存续期间动态改道，UI 的「从 💻 推送到 📱」就是一个 `route()` 调用。

### 3.3 与现有代码的映射

| 现状 | 目标 |
|---|---|
| `stross-kernel::relay`（Router 兼控制面+数据面） | 控制面在 `kernel` 门面（会话/路由/鉴权/凭证）；数据面转发下沉为「Transport 之上的转发器」，关键帧对齐逻辑原样保留 |
| `stross-kernel::Kernel`（内核门面） | **第七轮落地**：原 `StrossApp` 状态机与原 `kernel::Kernel` 骨架合并为单一 `Kernel`；命令面（`app_info` / `list_devices` / `start_relay` / `scan_relays` / `start_stream` / `stop_stream` / `stream_status` / `capture_status` / `start_receive` / `stop_receive` / `receive_status`）保持兼容 |
| `stross-endpoint::capture::CaptureBackend` | 演进为 `Source` 能力：增加 `descriptor()`（media kind / codecs / 分辨率上限），`start/stop/status` 语义不变 |
| `discovery.rs`（mDNS + TXT） | TXT 承载能力广播：`role=`、`transports=`、`codecs=`、控制端口 |
| `stross-proto`（v1 帧 + 控制消息） | v2 帧头（seq/分片）+ 协商/路由控制消息（见 §5） |

## 4. 传输层设计

### 4.1 可靠性契约

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReliabilityProfile {
    Lossless,  // TCP-like：控制消息、输入注入、剪贴板 —— 全序不丢
    Lossy,     // UDP-like：媒体帧 —— 允许丢帧，靠关键帧对齐自愈（现有机制）
    Adaptive,  // SRT-like：ARQ + 时延预算，超时则丢
}
```

### 4.2 Transport trait（签名草案）

```rust
pub struct TransportStats {
    pub rtt_ms: Option<u32>,
    pub loss_pct: Option<f32>,
    pub jitter_ms: Option<f32>,
    pub bytes_sent: u64,
    pub bytes_recv: u64,
}

pub struct SessionParams { pub session_id: SessionId, pub profile: ReliabilityProfile }
pub struct PeerAddr { pub transport: TransportId, pub addr: String }

#[async_trait]
pub trait Transport: Send + Sync + 'static {
    fn id(&self) -> TransportId;
    fn profile(&self) -> ReliabilityProfile;

    /// 发起方：连接对端并协商出一个数据会话。
    async fn connect(&self, peer: PeerAddr, params: SessionParams) -> Result<DataSession, TransportError>;
    /// 接收方：接受一个入站会话（WS 的 /ws/push、WebRTC 的 offer 都走这里）。
    async fn accept(&self, params: SessionParams) -> Result<DataSession, TransportError>;
    fn stats(&self) -> TransportStats;
}

/// 一条已建立的传输会话：负责把 Frame 映射到具体线格式。
pub struct DataSession { /* ... */ }
impl DataSession {
    pub async fn send_frame(&self, frame: Frame) -> Result<(), TransportError>;
    pub async fn recv_frame(&self) -> Result<Frame, TransportError>;
    pub async fn close(&self) -> Result<(), TransportError>;
}
```

职责边界：**分片/重组是传输实现的内部事务**——UDP 类传输用 v2 头的 `frag_idx/frag_cnt`
切大关键帧；WS 类整帧发送。内核与上层永远只看到完整 `Frame`。

### 4.3 传输协商

会话建立时：内核收集双方 `endpoints` ∩ 请求的 `profile` → 生成 `Offer` →
对端 `Answer` 选一个 → `DataSession` 就绪。控制消息永远走无损通道（复用 WS），
媒体按 profile 走协商出的传输。接收端按能力自动选传输（WebRTC 就绪前回退 WS）。

### 4.4 候选实现与优先级

| 传输 | profile | 状态 | 用途 |
|---|---|---|---|
| `transport-ws` | Lossless | ✅ 已落地（现状包一层） | 控制通道 + 媒体兜底 |
| `transport-webrtc` | Lossy | ✅ 阶段 1 已落地（str0m；control 可靠 / media 不可靠 datachannel） | 低延迟媒体通道，接收端 WebRTC（带 WS 回退） |
| `transport-srt` | Adaptive | ✅ 阶段 2 已落地（rsrt 纯 Rust，TSBPD/ARQ/零 C 依赖） | 弱网/跨 NAT 推流（relay `srt_port`；分片/重组用 v2 头 `frag_*`） |
| `transport-quic` | Lossless | ✅ 阶段 2 已落地（quinn 0.11 + rustls-ring；自签名证书） | 一条连接 control/media 双 stream 多路复用（relay `quic_port`），NAT 友好 |

`TransportStats` 直接喂给接收端 stats UI（`st-rate` / `st-latency`），不需要新 UI。

## 5. 协议演进（stross-proto v2）

**线格式（24 字节 v2 帧头 + JSON 控制消息 + 协商握手）的字段级定义见
[protocol.md](protocol.md)，此处只记录演进决策，不重复线格式。**

- 向后兼容策略：接收端与服务端同源升级、整体替换；v2 帧头在 WS 上取
  `seq=0, frag_cnt=0` 时语义等价 v1。不做线上双版本帧头转换（成本高、收益低）。
- `ControlMessage` 扩展（能力协商 `Capabilities/Offer/Answer`、路由 `Route`、
  会话事件 `SessionEvent`）与 `RoutePath` serde 表示（`{ kind: "direct"|"viaRelay"|"mesh" }`）
  已落入 stross-proto 与 protocol.md。

### 5.1 序列化格式选型（决策记录）

控制消息目前用 JSON（`serde_json`），媒体帧用定长二进制头。评估过 protobuf3，结论：

| 通信类型 | 现状 | protobuf3 的影响 |
|---|---|---|
| 媒体帧 | 24 字节定长头 + 载荷（载荷占 >99.5%） | **变差**：无固定线布局，每帧 varint 编解码 + 头更大，失去魔数快速校验 |
| 控制消息 | JSON（握手级，几百字节） | 体积减半但发生在握手瞬间，端到端零感知 |
| `/api/streams` | JSON（每 5s ~500B） | 无感 |

**决策**：

1. 媒体路径保持定长二进制头（见 protocol.md），不引入 protobuf；
2. 控制路径保持 JSON——可读性（日志可直接排查）> 体积；`ControlMessage` 已是 serde
   枚举，若未来需要二进制化，换 `postcard`（或 `rmp-serde`）只改一行
   （`serde_json::to_string` → `postcard::to_alloc_vec`），零 codegen；
3. protobuf 只在「控制面开放给多语言第三方」（独立 viewer、插件市场）时才值得评估——
   那是**契约管理**决策（`.proto` 机器可读、跨语言前后兼容），不是效率决策；
4. 阶段一不引入任何序列化框架改动。

### 5.2 前端 TypeScript（决策记录，2026-08 更新）

浏览器观看端（`stross-core/assets/viewer/`）已随 D1 移除；剩余一个前端
`apps/stross-gui/web/`（Tauri 壳），已从手写 JS 迁移为 **TypeScript 真源**
（`app.ts`）：

- **约束**：推流端前端由 index.html 直接加载——**cargo 构建必须零 node 依赖**；
- **方案**：前端目录一个 `tsconfig.json`（strict，`noEmitOnError`），
  `tsc` 发射的 `app.js` **提交进仓库**作为构建产物，index.html 不变；
- **开发流程**：改 `app.ts` 后运行 `npx tsc -p <目录>/tsconfig.json` 重新生成并提交两者
  （发射产物文件头注明来源）；类型检查 `npx tsc --noEmit -p <目录>/tsconfig.json`；
- **约束边界**：emitted JS 必须保持纯 JS 语法（不得出现 `!` 等 TS 专属运行时语法——
  使用 JSDoc 类型断言 `/** @type {T} */ (expr)` 或显式 `as` 表达式规避）；

### 5.3 阶段 2 决策记录（2026-08 更新）

阶段 2 的核心闭环（crate 拆分、Sink、控制面鉴权）已落地，以下按决策推迟：

| 项 | 决策 | 理由 |
|---|---|---|
| 拆 `stross-transport` crate | ✅ 落地 | 阶段 1 已证明抽象价值（同一 `handle_watch` 驱动 ws/webrtc）；传输实现的重依赖（str0m，未来 quic）不再进入 core/media/app 的依赖树；`stross_kernel::transport` / `stross_kernel::net` 路径 re-export 保持兼容 |
| `transport-srt` | ✅ 落地 | [rsrt 0.3](https://github.com/cesbo/rsrt) 是**纯 Rust SRT**（`#![deny(unsafe_code)]`，TSBPD/ARQ/HaiCrypt，依赖全为 RustCrypto/tokio，零 C 依赖，MIT/Apache-2.0）——推翻此前「无纯 Rust 实现」的过时结论；补上 `Adaptive` 可靠性契约（设计 §4.1 的第三个 profile），弱网/跨 NAT 推流通道；大帧按 v2 头 `frag_*` 分片/重组（SRT 单消息 ≤ 协商 MSS−44≈1456B），relay 开独立 UDP 端口（`RelayHandle::srt_port`），`RelayClient` 按 `srt://` scheme 选传输；集成测试证明同一 `handle_push` 驱动 ws/srt |
| `transport-quic` | ✅ 落地 | **quinn 0.11 默认 features 即 `rustls-ring`**（ring 只需 cc 无 cmake，本机满足，MIT/Apache-2.0、rust-version 1.85）——「接受 ring」前提成立即落地；线格式：control/media **两条双向 stream**（stream 即类型，无需消息类型前缀），长度前缀分帧（QUIC 流是 lazy 的，客户端 open 后发空消息作就绪信号），大帧整体发送（无单消息大小限制，不需要 `frag_*`）；自签名证书（rcgen，进程内一次）+ 客户端接受任意证书（局域网可信模型，与 ws:// 明文同级）；relay 开独立 UDP 端口（`RelayHandle::quic_port`），`RelayClient` 按 `quic://` scheme 选传输；集成测试证明同一 `handle_push` 驱动 ws/quic |
| 拆 `stross-kernel` crate | ✅ 已拆（第七轮） | `stross-core` 更名 `stross-kernel` 并吸收原 `stross-app` 全部服务：内核 = 所有平台无关服务，单一 `Kernel` 门面；平台适应独立 `stross-bridge`（见 docs/layering-architecture.md） |
| 控制面 WASM 插件（Extism） | ⏸ 推迟 | 设计自标「远期可选」；阶段 2 已落地 `AuthPolicy` trait + 内置 `PinAuthPolicy`（设计 §7 承诺的内置实现），WASM 只需实现同一 trait，不动内核 |
| 跨设备控制闭环（A 控制 B 推流） | ⏸ 推迟 | roadmap P0 级独立功能（远程控制 API + 鉴权 + UI），体量另立阶段；内核 `authorize`/`route` 命令面已为其留好接口 |

**Sink 状态**：`Sink` trait（§6.2）与首个实现 `RecordingSink`（录制：视频 Annex-B `.h264` + 音频 ADTS `.aac`，无外部依赖）已落地；原生播放器（Tauri 侧）、键鼠注入 / 剪贴板（Deskflow 方向，平台适配重且无法无头测试）留待后续能力插件。

## 6. 能力模型（Source / Sink）

### 6.1 Source（采集侧）—— `CaptureBackend` 的演进

```rust
pub trait Source: Send + Sync {
    fn descriptor(&self) -> CapabilityDescriptor;              // 新增：能力广播/协商用
    fn start(&self, cfg: &StreamConfig, tx: mpsc::Sender<Frame>) -> anyhow::Result<()>; // 不变
    fn stop(&self);                                            // 不变
    fn status(&self) -> CaptureStatus;                         // 不变
}
```

阶段 0 只给 `CaptureBackend` 加 `descriptor()`（默认实现返回「未知」即可，
避免强制所有实现改动），`FfmpegBackend` 与 Android `mobile.rs` 逐个补真实描述。

### 6.2 Sink（接收/注入侧）—— 新 trait

```rust
pub trait Sink: Send + Sync {
    fn descriptor(&self) -> CapabilityDescriptor;
    fn start(&self, rx: mpsc::Receiver<Frame>) -> anyhow::Result<()>; // 消费帧
    fn stop(&self);
}
```

阶段 1 的 Sink：接收端渲染（原生播放器，D6）、录制。
**Deskflow 方向衔接**：键鼠注入、剪贴板共享 = 坐在 `Lossless` profile 会话上的 `InputSink` / `ClipboardSink`，
复用同一套会话/路由/传输基座——架构上无新增概念，只有新能力插件。
安全上输入注入默认不启用（见 §7）。

## 7. 安全

- **会话级访问码（PIN）**：接收端可选的「访问码」（AirPlay 式），控制面强制校验，媒体数据面可选；
- **控制面强制鉴权**：`Route` / `SessionEvent` / 未来的跨设备控制 API 必须过鉴权；
- **能力最小化**：Sink 类能力（尤其输入注入）默认关闭，需用户在推流端显式开启；
- 鉴权策略做成内核接口（`AuthPolicy` trait），阶段 1 内置 PIN 实现，
  远期可换成 Extism/WASM 策略插件而不动内核。

## 8. 迁移路线（三阶段，已全部落地）

设计曾按三阶段推进（阶段 0 接口化 → 阶段 1 WebRTC 验证传输抽象 → 阶段 2 拆分与
能力扩展），**均已落地**：`stross-transport` crate 拆分、四传输
（ws/webrtc/srt/quic/memory）共用同一 `handle_push`/`handle_watch`、
`AuthPolicy`/`PinAuthPolicy` 控制面鉴权、`Sink` + `RecordingSink`。
各传输落地状态见 §4.4，决策记录见 §5.3，本页不再展开历史实施明细。

## 9. 明确不做的事（YAGNI 边界）

- ❌ element/pad 级媒体流水线插件（GStreamer 式）——插件粒度停在能力级；
- ❌ 阶段一动态加载（dylib / WASM）——只在阶段 2 的控制面策略上用 WASM；
- ❌ 线上双版本帧头转换——同源部署 + v2 字段默认值兼容即可；
- ❌ 「为抽象而抽象」：阶段 0 必须保证「只有一个 ws 实现也能跑」；
  在第二个传输实现（WebRTC）落地并跑通同一套测试之前，不扩展现有 trait。

## 10. 与 roadmap 的对应

| 本设计 | roadmap 条目 |
|---|---|
| Kernel / Router（传输方向控制） | P0 设备路由（类似投屏） |
| 能力广播（mDNS TXT 升级） | P0「mDNS 广播增强：TXT 携带角色/能力」 |
| 控制面/数据面分离 | P2 流解耦 |
| transport-webrtc | 「WebRTC 低延迟（<300ms）通道」 |
| Sink（原生播放器） | P1 手机端原生播放器 |
| InputSink / ClipboardSink | 远期 Deskflow 方向（新增） |
