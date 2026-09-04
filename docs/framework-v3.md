# 框架 v3：八概念 crate + 公共 trait 契约（设计规格）

> 状态：**设计定稿（待实施）**。
>
> **已与用户对齐的决策**：
> 1. **八概念提升为 crate 层级**——概念即 crate（契约 trait + 实现同仓），
>    kernel 只依赖各概念 crate 的 trait，不认识具体实现；
> 2. **领域层级保留 + 抽象解耦**——节点**拥有**端点（一对多，领域事实，
>    不是平级）；解耦发生在抽象层：端点 trait + 实现住在独立 `stross-endpoint`
>    crate（不塞进节点 crate），节点只持有端点**引用**（`Vec<EndpointId>`），
>    端点行为对象在平级端点表，「节点上的端点」是查询投影而非嵌套存储
>    （v2 把端点注册信息整个嵌进 `NodeRegistration` 结构是耦合根源，
>    本设计改为「节点持引用 + 端点表独立」）；
> 3. **完全不兼容策略**——v3 不做任何新旧兼容（无 `pub use` 兼容重导出、
>    无 wire 兼容字段、无旧方法面保留），壳层/kernel/协议全端同步演进；
> 4. **展示视图**：概念视图随仓（如 NodeInfo 随 stross-node）+ 跨概念
>    展示视图独立 `stross-view`；
> 5. **模块文件同步拆分**（大文件按领域拆小，见 §7）；
> 6. 自动化工具链**全部脚本迁移** Python + uv（§6）。
>
> 本文档是 v3 框架的唯一规格源。v1/v2 旧设计（分层手册 / 端点模型 / 通信
> 模式 / 插件架构 / 架构总览）按「单一真源」原则**废弃并归档**（§8 文档处置）。
> 关联：[protocol.md](protocol.md)（线协议，唯一保留的底层契约）。

## 1. 动机与目标

现状（v2）已具备雏形 trait（`Transport` / `Endpoint` / `ShareEndpoint` /
`SubscribeEndpoint` / `Loader` / `Interpreter` / `CaptureBackend`），但存在
结构性耦合：

1. **八概念不是平级公民**：传输、端点有 trait；**节点没有 trait**（散落
   `NodeInfo` / `NodeIdentity` / `TrustedNode` / `ScannedNode` 四个具体类型）；
   **共享 / 订阅只是端点 trait 的方法**而非独立概念；**发现是 kernel 内部
   模块**而非 trait；**序列化散落三处**（proto 帧头 + pick::Loader + endpoint
   codec）。
2. **注册表嵌套**：v2 `UnifiedRegistry` 是「节点 → 端点 → 策略」树形嵌套，
   本机端点走 `local` 表、远端端点走 `nodes` 表**双路径**——「统一注册表」
   名不副实；端点的身份依赖节点容器。
3. **内核耦合具体实现**：kernel 直接依赖 quinn（QUIC 中继 peer 循环）、
   axum（HTTP/WS）、mdns（发现）——「内核平台无关」只做到了 OS 层，没做到
   **依赖层**。
4. **壳层命令与内核门面纠缠**：Kernel 门面方法面过宽（会话/路由/鉴权/凭证/
   锚点/推流/接收/端点/身份/发现/协商），壳层直接调用深层实现。

**v3 目标**：

- 八概念各是一个 **crate**（`stross-node` / `stross-endpoint` / `stross-share`
  / `stross-subscribe` / `stross-transport` / `stross-serialize` / `stross-pick`
  / `stross-discovery`），公共 trait 在各自 crate 内定义，实现同仓；
- **领域层级保留 + 存储解耦**：节点**拥有**端点（一对多，领域事实）；
  解耦发生在抽象层——节点只持有端点**引用**（`Vec<EndpointId>`），端点
  行为对象在独立端点表（平级存储），「节点上的端点」是查询投影，不做
  树形嵌套（v2 把端点注册信息嵌进 `NodeRegistration` 是耦合根源）；
- 内核只依赖各概念 crate 的 trait + 注册表（`Box<dyn Trait>`），
  **不认识任何具体实现**；
- 壳层只依赖内核门面 + `stross-view` 展示视图类型，不触碰实现；
- 自动化工具链迁移到 Python + uv（§6）。

## 2. crate 布局与依赖（v3）

```text
crates/
  stross-proto        线协议 wire 类型（帧/控制消息/强类型 ID/枚举）——底座，零内部依赖
  stross-node         节点：拓扑主体（Node trait + NodeInfo/TransportAddr）
  stross-endpoint     端点：数据源/宿能力（Endpoint/ShareEndpoint/SubscribeEndpoint
                      + 数据契约 + screen/audio/file 实现）
  stross-share        共享：内容源侧编排（ShareService + 生命周期治理）
  stross-subscribe    订阅：接收方侧编排（SubscribeService + 接收编排）
  stross-transport    传输协议：Transport/DataSession + ws/srt/quic/webrtc/memory 实现
  stross-serialize    序列化协议：Loader/Unloader + Passthrough/Chunked 实现
  stross-pick         pick 规则：Interpreter/Pacing + RealtimePacing/StrictOrdered/JitterBuffer
  stross-discovery    广播发现：Discovery trait + mDNS/子网扫描实现
  stross-view         展示视图（跨概念共享：AppInfo/RelayInfo/StatusView/…）
  stross-kernel       ★ 编排器：只依赖上述概念 crate 的 trait + stross-view
  stross-bridge       平台适应（paths/hostname/平台端点构造）
apps/
  stross-cli / stross-gui / stross-relay   壳层（消费 kernel 门面 + stross-view）
```

依赖方向（单向无环，**概念 crate 互不依赖具体实现**）：

```text
stross-node / stross-transport / stross-serialize / stross-pick → stross-proto
stross-endpoint → stross-proto + stross-serialize + stross-pick（策略类型引用）
stross-share    → stross-proto + stross-endpoint（ShareEndpoint trait）
stross-subscribe→ stross-proto + stross-endpoint（SubscribeEndpoint trait）
stross-discovery→ stross-proto + stross-node（Node trait）
stross-view     → stross-proto（纯展示类型）
stross-kernel   → 全部概念 crate + stross-view（只经 trait 行动）
stross-bridge   → kernel + 概念 crate（产出参数，不持状态）
壳层            → kernel + stross-view
```

**概念 crate 的契约/实现同仓原则**：`stross-endpoint` 的 `Endpoint` trait
与 screen/audio/file 实现同在一个 crate；`stross-pick` 的 `Interpreter`
trait 与 `RealtimePacing`/`StrictOrdered` 实现同仓。**不设独立的
「契约 crate」**（概念即 crate，避免跨 crate 样板与「为抽象而抽象」）。

### 2.1 crate 模块边界（定稿）

每个概念 crate 内部按 `契约 / 实现 / 内部支撑` 分模块；**壳层只可见
crate 根的契约与视图，不见实现模块**（实现模块 `pub(crate)` 或文档隐藏）。

```text
stross-node            ├─ lib.rs：Node trait + NodeInfo/NodeRole/TransportAddr
                       └─（无实现：注册表条目即实现；扩展时拆 local/remote）

stross-endpoint        ├─ contract.rs：Endpoint/ShareEndpoint/SubscribeEndpoint/
                       │   MediaSourceEndpoint/StreamHost/FileHost/MediaHost/Runtime/
                       │   SubscribeCtx/TargetKind/EndpointClass/Probe/EndpointBase
                       ├─ data.rs：StreamConfig/VideoSource/AudioSourceConfig/
                       │   Quality/FilePushOptions + spawn_media_share/resolve_*
                       ├─ share/：screen/(linux/windows/macos/android)、audio、file、channel
                       ├─ subscribe/：media（Graph/Audio 族播放器）、file、channel
                       ├─ capture.rs / pipeline/ / playback/ / codec/ / convert/ / sources.rs
                       └─ factory.rs：平台端点工厂（bridge 调用，产出参数不持状态）

stross-share           ├─ lib.rs：ShareService/ActiveShare/ShareHandle（契约）
                       └─ impl/：publisher（发布/撤销+可见性）、lifecycle（watchers 治理）
                                  ——迁自 kernel active_shares/note_share_active

stross-subscribe       ├─ lib.rs：SubscribeService/SubscribeLink（契约）
                       └─ impl/：resolver（(节点,端点,策略)→SubscribeSpec 查表）、
                                  linker（链路启停/统计）、
                                  generator（★订阅端点生成，按 EndpointClass 分派工厂）
                                  ——迁自 kernel subscriber/generate_subscribe_endpoint

stross-transport       ├─ lib.rs：Transport/DataSession/SessionParams/PeerAddr/
                       │   SessionPacket/TransportError/TransportStats（契约，已同仓）
                       └─ ws/ srt/ quic/ webrtc/ memory/ relay_url/ net（实现）

stross-serialize       ├─ lib.rs：Loader/Unloader + SerializeRule（契约）
                       └─ impl/：passthrough（直通）、chunked（预留分包）
                                  ——迁自 kernel pick/load.rs

stross-pick            ├─ lib.rs：Interpreter/Pacing/FrameSink + PickRule（契约）
                       └─ impl/：realtime（RealtimePacing）、strict（StrictOrdered）、
                                  buffer（JitterBuffer）
                                  ——迁自 kernel pick/{interpret,manager,buffer}.rs

stross-discovery       ├─ lib.rs：Discovery/ScannedNode/StreamView/DiscoveryEvent（契约）
                       └─ impl/：mdns/（advertise/browse/select，内部用 crates/mdns fork）、
                                  scan（子网单播扫描+聚合）、view（ScannedNode 聚合构造）
                                  ——迁自 kernel discovery/{mdns,aggregate}.rs

stross-view            └─ lib.rs + channel/hostname/id/ports（纯展示类型，已建）

stross-kernel          ├─ kernel/：Kernel 门面 + registry.rs（★注册表：nodes/
                       │   endpoints 独立表，节点持引用）+ anchor/auth/id/view
                       ├─ relay/：中继服务器+HTTP 客户端+数据面（保留：内部服务）
                       ├─ negotiator/ + control.rs：协商/控制面（保留：内部服务）
                       ├─ channel/：对等通道（保留：内部服务）
                       ├─ file_xfer.rs：文件传输编排（保留，经 FileHost 契约暴露）
                       ├─ engine.rs：推流引擎（重组：实现 StreamHost 契约）
                       ├─ receiver.rs：接收编排（重组：实现 MediaHost 契约）
                       ├─ bootstrap.rs / settings.rs / error.rs / lock.rs（保留支撑）
                       └─ 迁出：pick/→serialize+pick；discovery/→stross-discovery；
                                 subscriber/→stross-subscribe 实现；active_shares
                                 →stross-share 实现；sender/watch 留 kernel 作内部 util
```

### 2.2 策略注册表模式（ID 运行时匹配 → trait 对象表）

**相似对象抽同一 trait，运行时按 ID 查表装载策略**——这是全框架的通用
扩展机制，凡「按 ID 选实现」的地方一律用注册表 + `Box<dyn Trait>`，禁止
`match` 硬编码具体类型（原 `generate_subscribe_endpoint` 硬编码
`FileReceiveEndpoint`/`MediaReceiveEndpoint` 即反面教材）：

| ID 键 | 注册表 | trait 对象 | 装载时机 |
|-------|--------|-----------|----------|
| `EndpointId` | 端点注册表 | `Box<dyn ShareEndpoint>` | 端点挂载/load |
| `EndpointClass` | 订阅端点生成工厂 | `Box<dyn Fn(...) -> Box<dyn SubscribeEndpoint>>` | 订阅解析后 |
| `TransportId` | 传输注册表 | `Box<dyn Transport>` | 按 relay URL scheme |
| `SerializeRule` | 序列化工厂 | `Box<dyn Loader>` / `Box<dyn Unloader>` | 流建立时 |
| `PickRule` | pick 工厂 | `Box<dyn Interpreter>` / `Box<dyn Pacing>` | 流建立时 |
| `NodeId` | 节点注册表 | `Box<dyn Node>` | 发现/目录入库 |
| `StrategyId` | 策略注册表 | `EndpointStrategy` 值 | 协商定稿 |

共性：**键是强类型 ID，值是 trait 对象，表由内核持有，实现方注册**。
新增一种策略 = 实现 trait + 注册，不碰内核分派逻辑。

## 3. 八概念契约

### 3.1 节点（stross-node）

```rust
/// 网络拓扑中的互联主体（手机/电脑/中继…）。
pub trait Node: Send + Sync {
    fn id(&self) -> NodeId;
    fn name(&self) -> &str;
    /// 角色（可作源 / 可作汇 / 中继 / 控制者）。
    fn roles(&self) -> &[NodeRole];
    /// 能力描述（发现/协商用）。
    fn caps(&self) -> &[CapabilityDescriptor];
    /// 可达地址（传输候选）。
    fn addrs(&self) -> &[TransportAddr];
}

/// 节点视图（发现/图聚合产物，跨壳层展示用）。
pub struct NodeInfo {
    pub node_id: NodeId,
    pub name: String,
    pub roles: Vec<NodeRole>,
    pub caps: Vec<CapabilityDescriptor>,
    pub addrs: Vec<TransportAddr>,
}
```

**节点拥有端点（领域层级），但持有的是引用**：节点是拓扑宿主，声明它
拥有哪些端点（`endpoint_ids: Vec<EndpointId>`）；端点行为对象（trait 对象 +
策略 + 状态）在**独立端点表**（平级存储）。「节点上的端点」= 按节点
`endpoint_ids` 查端点表（查询投影），**不是把端点注册信息嵌进节点结构**
（v2 的 `NodeRegistration { endpoints: HashMap<…> }` 嵌套是耦合根源）。

### 3.2 端点（stross-endpoint）

```rust
/// 端点公共视图（注册表与 UI 按它展示）。
pub trait Endpoint: Send + Sync {
    fn id(&self) -> EndpointId;
    fn kind(&self) -> MediaKind;
    fn name(&self) -> &str;
    fn class(&self) -> EndpointClass;
    fn target(&self) -> TargetKind;
    fn transport_profile(&self) -> ReliabilityProfile;
    fn strategy(&self) -> EndpointStrategy;
}

/// 分享端点（内容源）：可被订阅。
pub trait ShareEndpoint: Endpoint {
    fn available(&self) -> bool;
    fn last_error(&self) -> Option<&str>;
    fn load(&mut self) -> StdResult<(), String>;
    fn share(&self, host: Arc<dyn ShareHost>, runtime: Arc<dyn Runtime>, ctx: SubscribeCtx);
}

/// 订阅端点（内容宿）：主动订阅并处理。
pub trait SubscribeEndpoint: Endpoint {
    fn subscribe(&self, host: Arc<dyn SubscribeHost>, runtime: Arc<dyn Runtime>, spec: SubscribeSpec);
}
```

**内核调度能力 = 四个小 trait（取代旧聚合 `EndpointApp`）**——端点只拿自己
需要的能力，不见内核整张脸；每个 trait 对象安全、面最小：

```rust
#[async_trait]
pub trait StreamHost: Send + Sync {              // 媒体推流能力
    async fn start_stream(&self, cfg: StreamConfig, relay_url: Option<String>)
        -> anyhow::Result<StartResult>;
    fn relay_port(&self) -> Option<u16>;
}
#[async_trait]
pub trait FileHost: Send + Sync {                // 文件传输能力
    async fn push_file(&self, path: PathBuf, opts: FilePushOptions) -> anyhow::Result<u64>;
    async fn receive_file(&self, watch_url: String, stream_id: StreamId, out_dir: PathBuf)
        -> anyhow::Result<ReceivedFile>;
}
#[async_trait]
pub trait MediaHost: Send + Sync {               // 媒体接收能力
    async fn receive_media(&self, spec: &SubscribeSpec) -> anyhow::Result<u64>;
}
pub trait Runtime: Send + Sync {                 // 运行时载体（端点自驱动）
    fn spawn_task(&self, fut: Pin<Box<dyn Future<Output = ()> + Send>>);
}

/// 分享端可见能力组合：ShareHost = StreamHost + FileHost（组合 trait，blanket
/// impl 自动覆盖；媒体端点只用 StreamHost 部分——start_stream/relay_port，
/// 文件端点用 FileHost 部分——push_file；§3.2 对字面签名的唯一微调）。
pub trait ShareHost: StreamHost + FileHost {}
impl<T: StreamHost + FileHost> ShareHost for T {}

/// 订阅端可见能力组合：SubscribeHost = MediaHost + FileHost（与 ShareHost 同一
/// 模式；媒体订阅端用 MediaHost 部分——receive_media，文件订阅端用 FileHost
/// 部分——receive_file）。
pub trait SubscribeHost: MediaHost + FileHost {}
impl<T: MediaHost + FileHost> SubscribeHost for T {}
```

- 分享端只见 `ShareHost（StreamHost + FileHost）+ Runtime`：媒体端点只用
  `StreamHost` 部分（start_stream/relay_port）、文件端点用 `FileHost` 部分
  （push_file）；订阅端只见 `SubscribeHost（MediaHost + FileHost）+ Runtime`：
  媒体订阅端用 `MediaHost` 部分（receive_media）、文件订阅端用 `FileHost`
  部分（receive_file）。
- 生命周期治理（watchers=0 自动收尾 / 取消通告联动停止）**从 trait 方法
  挪进 `stross-share::ShareService`**（`on_subscribed` 登记、`reap_if_unwatched`
  复查、`stop` 显式停止）——端点不再回调内核生命周期，`note_share_active` /
  `stop_share_if_unwatched` 从契约删除。
- pull 模式流 id 协商定稿即实际流 id（`ctx.stream_id`），`ShareService` 登记
  无需端点回传。

端点表（平级存储；节点只持引用）：

```rust
struct EndpointRegistry {
    endpoints: HashMap<EndpointId, EndpointEntry>, // 独立表，不嵌套在节点下
}
struct EndpointEntry {
    ep: Box<dyn ShareEndpoint>,
    owner: NodeId,            // 归属节点（关联字段，供「节点→端点」投影查询）
    published: bool,
    visibility: Visibility,
    strategies: Vec<EndpointStrategy>, // 端点声明，策略独立索引
    state: EndpointState,
    subscribers: u32,
}
```

领域层级不变：一个节点拥有多个端点（`endpoint_ids` 引用列表）；端点表是
平级存储，端点行为对象不依赖节点容器而存在。

**注册表 = ID → 展示元数据 映射（强类型 ID 铁律的承载机制）**：业务逻辑
全程只碰强类型 ID（`NodeId` / `EndpointId` / `StreamId` …），**零裸 String
作 key / id**。任何「必须出现的字符串」（端点名 / 节点名 / 标题 / 能力描述）
一律收进注册表的展示元数据字段（`EndpointEntry.name`、`NodeEntry.name` 等），
**不进 wire、不散落各处**；展示层经注册表按 ID 查字符串，节点间经发现/目录
（`DiscoveryInfo` / `LocalCatalog`）共享注册表快照。ID 是身份，字符串是身份的
展示映射——二者解耦，字符串永不充当标识符。

### 3.3 共享（stross-share）

```rust
pub trait ShareService: Send + Sync {
    fn publish(&self, ep: &dyn ShareEndpoint, visibility: Visibility) -> Result<ShareHandle, String>;
    fn unpublish(&self, handle: &ShareHandle) -> Result<(), String>;
    fn on_subscribed(&self, ep: &dyn ShareEndpoint, ctx: &SubscribeCtx, endpoint_id: EndpointId);
    fn reap_if_unwatched(&self, stream: &StreamId);
    fn stop(&self, endpoint_id: EndpointId) -> Result<(), String>;
    fn active(&self) -> Vec<(StreamId, ActiveShare)>;
}
```

从 kernel 的 `active_shares` / `note_share_active` / `stop_share_if_unwatched`
逻辑抽提；**内核实现本契约**（`impl ShareService for Kernel`，放
stross-kernel `kernel/share_service.rs`；契约 crate 只放 trait 与纯类型，
依赖方向 kernel → 契约单向——壳层经 Kernel 门面调契约方法，端点层只经
`ShareEndpoint` 契约被回调）。登记条目统一为契约类型 `stross_share::ActiveShare`
（§7.1 类型去重，删除 kernel 旧定义；订阅者节点集投影自注册表）。

### 3.4 订阅（stross-subscribe）

```rust
pub trait SubscribeService: Send + Sync {
    fn resolve(&self, node: &NodeId, endpoint: EndpointId, strategy: Option<&StrategyId>)
        -> Option<SubscribeSpec>;
    fn subscribe(&self, spec: SubscribeSpec, sink: Box<dyn SubscribeEndpoint>) -> Result<LinkId, String>;
    fn unsubscribe(&self, link: &LinkId) -> Result<(), String>;
    fn links(&self) -> Vec<SubscribeLink>;
}
```

从 kernel 的 `subscriber` / `receiver` / `generate_subscribe_endpoint` 抽提；
**内核实现本契约**（`impl SubscribeService for Kernel`，放 stross-kernel
`kernel/subscribe_service.rs`；契约 crate 只放 trait 与纯类型，依赖方向
kernel → 契约单向——壳层经 Kernel 门面调契约方法）。订阅端点生成按
`EndpointClass` 查工厂注册表（§2.2，`generate_subscribe_endpoint` 去硬编码）。

### 3.5 传输协议（stross-transport）

```rust
pub trait Transport: Send + Sync + 'static {
    fn id(&self) -> TransportId;
    fn profile(&self) -> ReliabilityProfile;
    async fn connect(&self, peer: &PeerAddr, params: &SessionParams)
        -> Result<Box<dyn DataSession>, TransportError>;
    async fn accept(&self, params: &SessionParams)
        -> Result<Box<dyn DataSession>, TransportError>;
    fn stats(&self) -> TransportStats;
}
pub trait DataSession: Send + Sync + 'static {
    async fn send(&self, pkt: SessionPacket) -> Result<(), TransportError>;
    async fn recv(&self) -> Result<Option<SessionPacket>, TransportError>;
    async fn close(&self) -> Result<(), TransportError>;
    fn peer_addr(&self) -> Option<std::net::SocketAddr> { None }
}
```

与现有 `stross-transport` 一致（实现零改动迁移）；契约本就在该 crate。

### 3.6 序列化协议（stross-serialize）

```rust
pub trait Loader: Send {
    fn serialize_rule(&self) -> SerializeRule;
    fn load(&self, track: TrackInfo, data: &[u8], pts_ms: u32) -> Vec<Frame>;
}
pub trait Unloader: Send {
    fn serialize_rule(&self) -> SerializeRule;
    fn unpack(&mut self, frame: Frame) -> Option<Vec<u8>>;
}
```

合并现状三处：proto 帧头（线格式，留在 proto）、kernel `pick::Loader`
（装载框架，迁入 stross-serialize）、endpoint codec（NAL/ADTS 切帧，作为
`Passthrough` 的载荷准备）。`SerializeRule` 枚举定义留在 proto。

### 3.7 pick 规则（stross-pick）

```rust
pub trait Interpreter: Send {
    fn rule(&self) -> PickRule;
    fn push(&mut self, frame: Frame);
    fn poll(&mut self) -> Option<Frame>;
}
pub trait Pacing: Send {
    fn rule(&self) -> PickRule;
    fn emit(&self, frame: Frame, sink: &mut dyn FrameSink);
}
```

从 kernel `pick/` 迁入实现（`RealtimePacing` / `StrictOrdered` / `JitterBuffer`）；
`PickRule` 枚举定义留在 proto。

### 3.8 广播发现（stross-discovery）

```rust
pub trait Discovery: Send + Sync {
    fn browse(&self) -> Vec<ScannedNode>;
    fn advertise(&self, enabled: bool);
    fn probe(&self, addr: &str) -> Option<ScannedNode>;
}
```

从 kernel `discovery/`（mDNS 浏览/广播/聚合）+ `mdns` crate 迁出为发现实现；
内核持有 `Vec<Box<dyn Discovery>>`，事件聚合进 KernelEvent。

### 3.9 概念操作语义总表（特性清单）

每个概念的 trait 对应一组**最小操作语义**；语义命名以数据流动词为准
（不按传输实现命名），便于策略扩展时保持接口稳定：

| 概念 | 操作语义 | 契约映射 | 说明 |
|------|---------|---------|------|
| 传输 | **link**（建链） | `Transport::connect` / `accept` | 发起/接受一条数据会话 |
| 传输 | **put**（发送） | `DataSession::send` | 整包投递（分片是传输内部事务） |
| 传输 | **get**（接收） | `DataSession::recv` | 阻塞取包；`None` = 干净关闭 |
| 传输 | **hold**（暂停/流控） | **不新增** | 背压由各实现内部处理（bounded
  channel / QUIC 流控 / SRT 拥塞），上层 pick 层节流；不把流控协议硬编码
  进传输接口，避免 SRDL/QUIC/WS 语义差异泄漏到公共 trait |
| 序列化 | **load**（装载） | `Loader::load` | 原始数据 → 线上帧（打包/分包） |
| 序列化 | **unload**（解装载） | `Unloader::unpack` | 帧 → 原始数据（重组/校验） |
| pick | **upload**（发送侧调度） | `Pacing::emit` | 按规则打发送节奏（直通/节流） |
| pick | **download**（接收侧解读） | `Interpreter::push/poll` | 按规则还原帧流（Realtime/
  StrictOrdered） |
| pick | **hold**（缓冲/暂停） | `JitterBuffer`（stross-pick 内部） | 有损路径落缓冲、按 PTS 调度；
  StrictOrdered 无损路径直通不缓冲 |
| 发现 | **browse**（浏览） | `Discovery::browse` | 当前已知节点快照 |
| 发现 | **advertise**（广播） | `Discovery::advertise` | 开启/关闭可被发现 |
| 发现 | **probe**（探测） | `Discovery::probe` | 手动地址加入/可达性 |
| 共享 | **publish** / **unpublish** | `ShareService::publish/unpublish` | 通告/撤销「可被订阅」 |
| 共享 | **reap**（收尾） | `ShareService::reap_if_unwatched` | watchers=0 复查收敛 |
| 订阅 | **resolve**（解析） | `SubscribeService::resolve` | (节点,端点,策略) → SubscribeSpec |
| 订阅 | **link**（建链） | `SubscribeService::subscribe/unsubscribe` | 订阅链路启停（多链路互不级联） |

原则：**接口只表达「要什么」，不表达「怎么实现」**——hold/流控这类
实现相关的语义留在实现内部，公共 trait 不因单一传输（如 SRT 弱网重传）
的机制而膨胀。

## 4. 内核（编排器，stross-kernel）

```rust
pub struct Kernel {
    nodes: NodeRegistry,              // node_id → Box<dyn Node>（含 endpoint_ids 引用）
    endpoints: EndpointRegistry,      // endpoint_id → EndpointEntry（独立存储，节点持引用）
    shares: Box<dyn ShareService>,    // 共享编排
    subscribes: Box<dyn SubscribeService>, // 订阅编排
    transports: TransportRegistry,    // transport_id → Box<dyn Transport>
    discoveries: Vec<Box<dyn Discovery>>,
    auth: Arc<dyn AuthPolicy>,
    events: broadcast::Sender<KernelEvent>,
}
```

- 门面方法面按八概念归组：`start`（锚定+发现+协商端点）、`node` 查询、
  `share` 发布/撤销、`subscribe` 建链/停链、`event` 订阅、`info` 视图；
- 旧方法面（create_session / route / authorize / issue_share_token /
  start_stream / start_receive …）**直接收敛删除**——完全不兼容策略下
  壳层随 v3 一起改，不做薄封装过渡。

## 5. 与现有代码的迁移映射

| 现状 | v3 归属 | 状态 |
|---|---|---|
| `stross-types::contract`（Endpoint SPI + 数据契约） | 迁 `stross-endpoint`（契约+实现同仓） | ✅ P1 已迁（contract.rs + data.rs 真定义） |
| `stross-types`（视图/DTO/端口/channel/hostname） | 展示视图迁 `stross-view` | ✅ P1 已迁并删除 crate |
| `stross-types::id`（重导出 proto ID） | 直接引用 `stross-proto` | ✅ P1 已删（id 真源在 proto） |
| `stross-transport`（Transport/DataSession/RelayUrl/net） | 保留（契约+实现已同仓） | ✅ session_id 已强类型化 |
| kernel `pick/`（Loader/Interpreter/JitterBuffer） | 迁 `stross-serialize` + `stross-pick` | 🔄 P2a 进行中 |
| kernel `discovery/` + `mdns` | 迁 `stross-discovery` | ⏳ P2b |
| kernel `engine`/`sender`/`watch`/`receiver`/`subscriber` | 重组为 `stross-share` / `stross-subscribe` 实现 | ⏳ P2c |
| kernel `negotiator`/`control`/`channel`/`file_xfer` | 保留在 kernel（内部服务），只依赖概念 trait | ⏳ P2 |
| kernel `kernel::endpoint`（UnifiedRegistry 嵌套） | **存储解耦**：节点表持端点引用（`endpoint_ids`），端点行为对象独立表（owner 关联），「节点→端点」为查询投影 | ✅ P2e 已完成，验证全绿 |
| 壳层 CLI/GUI/relay | 只调 Kernel 门面 + stross-view 类型 | ✅ P3 已完成（discovery 薄壳删除，壳层直连 stross-discovery / stross-transport；relay client 深层路径暂留并注释「P3 后清理」） |

## 6. 自动化工具链（Python + uv）

**目标**：废弃手工 shell 脚本（scripts/*.sh、*.mjs），迁移为 uv 管理的
Python 工具链——一个命令入口、显式环境、跨平台。

```text
pyproject.toml            # uv 管理的项目
scripts/
  __init__.py
  cli.py                  # uv run python -m scripts cli|check|test|phone|android…
  commands/
    build.py              # 原 build.sh（cli/relay/gui/android + release/debug）
    check.py              # 原 check.sh（fmt/clippy/test/tsc/jsdom 门禁）
    test_e2e.py           # 原各 *_test.sh（quic-stale/srt-silence/share-token/dual-node…）
    phone.py              # 原 phone-cdp.mjs（dump/text/click/eval）
    frontend.py           # 原 test-frontend.mjs（jsdom 无头测试）
    android.py            # 原 setup-android.sh（JAVA_HOME/NDK 约束收敛）
    hooks.py              # 原 install-hooks.sh
```

用法示例：

```bash
uv run python -m scripts check --quick
uv run python -m scripts build android --release
uv run python -m scripts phone dump
uv run python -m scripts e2e dual-node-file
```

迁移原则：**行为等价**，逐脚本迁移 + 对照验证，不夹带逻辑修改；迁移完成
后删除 scripts/*.sh / *.mjs（git 历史可查）。

## 7. 模块文件拆分

| 大文件（现状） | 拆分去向 |
|---|---|
| `stross-endpoint/src/contract.rs`（端点 SPI + 数据契约，原 stross-types 619 行并入） | ✅ 已分 contract.rs（trait）+ data.rs（数据契约）+ 宏留 contract.rs；P4 按需细拆 |
| `stross-kernel/src/kernel/mod.rs`（1168 行） | 按域拆（已在做）：anchor / streams / receive / session_api / endpoint_api 进一步细拆 | ✅ P3：tests 移出 kernel_tests.rs，mod.rs 只留 Kernel 结构 + 构造 + 门面委托/事件广播 |
| `stross-kernel/src/kernel/endpoint.rs`（1258 行） | 注册表拆分：endpoint_registry.rs / strategy.rs / subscribe_generate.rs | ✅ P3：拆为 kernel/endpoint/（mod.rs 端点表核心 + registry.rs 统一注册表 + strategy.rs 策略解析 + subscribe_generate.rs 工厂表 + file_source.rs） |
| `stross-proto/src/message/ids.rs`（866 行） | 拆为 transport.rs（TransportId/ReliabilityProfile）/ media.rs（MediaKind/CodecId）/ node.rs（NodeId）/ stream.rs（StreamId/StreamKey） | ✅ P3：拆为 ids/（transport.rs / media.rs / node.rs / stream.rs / derive.rs），`stross_proto::message::*` 路径经 mod 重导出不变 |
| `stross-kernel/src/discovery/mdns.rs`（476 行） | 随 stross-discovery 拆分：advertise.rs / browse.rs / select.rs |
| `stross-kernel/src/relay/` 各文件 | 按 server/client/data_plane/peers 保持，内部细拆 |

### 7.1 类型重名去重清单（P2/P3 实施依据）

v3 迁移中发现以下类型在多个 crate 重复定义（契约 crate 为真源，旧定义删除）：

| 类型 | 重复位置 | 处置 |
|---|---|---|
| `ScannedNode` / `StreamView` / `to_views` | kernel `discovery/aggregate.rs` + `stross-discovery`（契约） | P2b：删 kernel 定义，统一 stross-discovery |
| `ReceiveStats` | kernel `receiver.rs`（多 `audio_blocks_in` 字段）+ `stross-view` | ✅ P2e：统一 stross-view（合并 `audio_blocks_in`/`paced_*`/`error` 字段；kernel 旧定义删除，经 `stross_view::ReceiveStats` 引用） |
| `RelayInfo` | kernel `relay/dto.rs` 是 `RelayInfoResp`（HTTP API 响应，SRT/QUIC 端口），stross-view 是 `RelayInfo`（设备卡片视图）——**非同名**，各自保留 | 无需去重 |
| `KernelEvent` | kernel `kernel/mod.rs`（含 SessionStarted/Routed/Ended 旧变体）+ `stross-view`（八概念变体） | ✅ P2e：统一 stross-view 八概念变体（旧会话变体随方法面收敛删除；kernel 重导出，session 事件广播移除，内部事件改发 `stross_view::KernelEvent`） |
| `CameraEndpoint` | `stross-view`（DTO）+ `stross-endpoint::sources`（重导出） | ✅ P1 已统一（sources 重导出 stross-view） |
| `ActiveShare` | kernel `kernel/mod.rs`（endpoint_id + delivery）+ stross-share（契约，含 subscriber_nodes） | ✅ P2d：删 kernel 定义，统一 stross-share（订阅者节点集投影自注册表） |

## 8. 文档处置（丢弃旧设计）

| 文档 | 处置 |
|---|---|
| `layering-architecture.md` | **废弃** → 分层判据并入本文档 §2（v3 单一真源） |
| `endpoint-model-v2.md` | **废弃** → 端点概念并入本文档 §3.2（节点持端点引用、端点独立表取代嵌套注册表） |
| `comm-mode-v2.md` | **废弃** → 数据管道模型并入本文档 §3.6/§3.7 |
| `plugin-architecture.md` | **废弃** → 传输基座并入本文档 §3.5 |
| `architecture.md` | **废弃** → 交互模型并入本文档 |
| `requirements.md` / `roadmap.md` | 保留（需求与路线不随框架变） |
| `protocol.md` | **保留**（线协议唯一真源） |
| `platforms.md` / `android-build.md` / `dev-playbook.md` | 保留（操作指南与坑位） |
| `iteration-plan.md` | 保留（迭代日志） |
| `docs/README.md` | 更新登记表（删除废弃条目，登记 framework-v3） |

## 9. 实施阶段

| 阶段 | 内容 | 验收 |
|---|---|---|
| P1 | 建八概念 crate 骨架（node/endpoint/share/subscribe/transport/serialize/pick/discovery + view）；stross-types 拆解删除；transport session_id 强类型化 | ✅ cargo check/clippy 全绿 |
| P2a | kernel `pick/` → stross-serialize + stross-pick 实现迁出（Loader/PassthroughLoader/loader_for；RealtimePacing/StrictOrdered/JitterBuffer/StreamChannel/InterpretRegistry） | ✅ 已完成，验证全绿 |
| P2b | kernel `discovery/` → stross-discovery 实现迁出（mDNS 广播/浏览 + 子网扫描聚合；`Discovery` struct 改名避让契约 trait） | ✅ 已完成，验证全绿（kernel 薄壳兼容层 P3 删） |
| P2c | **EndpointApp 拆四能力 trait**（StreamHost/FileHost/MediaHost/Runtime + 组合 ShareHost，§3.2）：stross-endpoint 契约与全部实现（share/subscribe/data/宏）+ kernel 的 EndpointApp impl 同步拆分；note_share_active/stop_share_if_unwatched 从契约删除（内核保留自有方法，P2e 迁 ShareService） | ✅ 已完成，验证全绿 |
| P2d | stross-share/stross-subscribe 实现（publisher/lifecycle 迁自 active_shares；resolver/linker/generator 迁自 subscriber + generate_subscribe_endpoint 去硬编码）——**基于 P2c 后的 host 签名** | ✅ 已完成，验证全绿（impl ShareService/SubscribeService for Kernel；EndpointClass 工厂注册表；ActiveShare 统一；协商层 notify_subscribed 接回 active_share 登记） |
| P2e | kernel 注册表解耦（节点持端点引用 + 独立端点表）+ 生命周期接 ShareService + §7.1 类型去重（ReceiveStats/KernelEvent）——**旧方法面收敛拆分**：死方法本次删（`issue_share_token` 无调用方），活方法（会话/凭证/推流/接收类）P3 随壳层迁移一并收敛 | ✅ 已完成，验证全绿（check/clippy 零警告；415 workspace 测试全绿） |
| P3 | 壳层命令面随 v3 全量迁移（CLI/GUI/relay 改调新 API，无旧路径）+ 大文件拆分 + 类型去重（§7.1） | ✅ 已完成，验证全绿（discovery 薄壳删除、壳层直连概念 crate；Kernel 旧方法面收敛为 pub(crate)/删除；§7 大文件拆分完成；check/clippy 零警告；415 workspace 测试全绿） |
| P4 | 旧文档废弃（layering-architecture/architecture/endpoint-model-v2/plugin-architecture/comm-mode-v2）+ docs/README 登记 + AGENTS.md 更新 | 文档单一真源 |
| P5 | Python+uv 工具链迁移（逐脚本行为等价） | ✅ 主体完成（16 .sh 删、9 命令建），验证并入 P6 |
| P6 | 全量回归：cargo test + clippy + tsc + jsdom + uv check --quick + build cli | 全绿 |
| P7 | 一次性大提交（重构完成） | ✅ `908bdd9` |

## 10. v3.1 深化：端点即插件（Endpoint-as-Plugin）

> 状态：**设计定稿（待实施）**。
>
> 用户诉求（重构完成后的下一轮）：**核心是减少技术债 + 按概念直观管理代码**；
> 节点与端点因领域层级无法展平，应**通过提取 trait 解耦端点与节点**——端点在
> 体验上就是节点的插件，最终应像「节点插件区」一样组织。继续完全不兼容策略。

### 10.1 动机（v3 落地后暴露的技术债）

1. **stross-node 是死 crate**：`Node` trait 全仓零消费者（`stross-discovery`
   声明依赖但代码不用；kernel `graph.rs` 重复定义与 stross-node **完全相同**的
   `NodeInfo` / `NodeRole` / `TransportAddr`——§7.1 去重表漏了这组）；
2. **扁平端点表遮蔽（层级展平的代价）**：`EndpointRegistry` 以 `EndpointId`
   为键，`resolve_strategy` / `stream_profile` / `SubscribeService::resolve`
   查表时**忽略 node_id**（`let _ = node_id;`）——跨节点同 id（如远端
   `screen:0`）会遮蔽本机/其它节点条目；wire 上 `(node_id, endpoint_id)` 本
   来就是双字段，内部存储却把层级压平了；
3. **壳层穿透 kernel 内部模块**：GUI `probe_relay` 与 CLI `adb status` 直调
   `stross_kernel::relay::client::{probe_base, info, streams}`（注释「P3 后
   清理」滞留）；
4. **可见性未收紧**：`verify_share_token` / `token_validator` 为 `pub` 但仅
   测试消费。

### 10.2 设计决策

| # | 决策 | 内容 |
|---|------|------|
| D1 | **端点 = 节点插件（契约语义定稿）** | `Endpoint` trait 即插件契约：自描述身份 + 能力声明 + 挂载性探测（`load`）+ 行为（`share`/`subscribe`）；四能力 trait（`StreamHost`/`FileHost`/`MediaHost`/`Runtime`）+ 组合（`ShareHost`/`SubscribeHost`）= 插件可见的**宿主能力接口**。插件只依赖宿主能力 trait + 强类型 ID，不认识具体节点/内核类型（v3 §3.2 已满足，本轮语义文档化） |
| D2 | **节点 = 插件宿主（激活 stross-node）** | `Node` trait 去掉 `endpoints()`（插件区清单是注册表投影，不属节点行为）；删 kernel `graph.rs` 重复类型 → 统一 `stross_node::{NodeInfo, NodeRole, TransportAddr}`；`impl Node for NodeInfo`（视图 DTO 实现行为契约，「注册表条目即实现」）；`Kernel::upsert_node` 泛型化 `upsert_node<N: stross_node::Node>(&self, node: N)`——任何节点形态实现 `Node` 即被内核接纳（发现扫描结果、目录映射、本机能力同一入口） |
| D3 | **插件挂载表（层级进地址）** | proto 增 `EndpointRef { owner: NodeId, endpoint: EndpointId }`（强类型复合定位，不进 wire——wire 已是双字段）；`EndpointRegistry` 存储键 `HashMap<EndpointId, …>` → `HashMap<EndpointRef, …>`；`EndpointEntry.owner` 字段删除（已入键）；**全部读路径节点限定**，`resolve_strategy` / `stream_profile` / `resolve` 按 `(node, endpoint)` 精确取——**修复跨节点遮蔽** |
| D4 | **RelayClient 服务对象** | `relay::client` 自由函数 → `RelayClient` 结构（持默认超时），方法 `probe_base` / `info` / `streams`；壳层两处直调改服务对象；删「P3 后清理」注释。中继 HTTP 客户端仍是 kernel 内部服务（响应契约单一真源在 kernel），壳层只消费服务对象，不手写客户端 |
| D5 | **可见性收紧** | `verify_share_token` 收敛为模块自由函数（校验单一真源，数据面校验器 `KernelTokenValidator` 复用）；`token_validator` 保留 `pub` + `#[doc(hidden)]`（数据面接线原语，集成测试独立接线场景需要公开构造路径） |
| D6 | **概念命名对齐** | 注册表模块按「插件挂载表」语义重写注释（`UnifiedRegistry` 改名不动——避免无谓 churn，注释与 API 文档表达插件语义） |

### 10.3 端点插件契约（stross-endpoint，语义定稿）

- 插件（端点）**不知道宿主是谁**：`share`/`subscribe` 收宿主能力对象
  （`Arc<dyn ShareHost>` / `Arc<dyn SubscribeHost>`）+ 运行时载体
  （`Arc<dyn Runtime>`），宿主身份只以强类型 `NodeId` 出现在载荷
  （`SubscribeCtx.subscriber`）与定位（`EndpointRef.owner`）；
- 插件的挂载性 = `load` 探测（`available` / `last_error`）；挂载表只做身份
  登记与通告参数管理，不持有插件实现知识；
- **新增插件 = 实现 `Endpoint` + 注册**（`Kernel::seed_endpoint`），内核分发
  零改动（§2.2 策略注册表模式）。

### 10.4 节点插件宿主（stross-node，激活）

```rust
/// 网络拓扑中的互联主体（手机/电脑/中继…）= 端点插件的宿主。
pub trait Node: Send + Sync {
    fn id(&self) -> NodeId;
    fn name(&self) -> &str;
    fn roles(&self) -> &[NodeRole];
    fn caps(&self) -> &[CapabilityDescriptor];
    fn addrs(&self) -> &[TransportAddr];
    // 无 endpoints()/plugins()：插件区清单是注册表投影，不属节点行为
}
impl Node for NodeInfo { /* 视图 DTO 即节点快照；字段直投影 */ }
```

- kernel `graph.rs` 删除 `NodeRole` / `TransportAddr` / `NodeInfo` 重复定义，
  统一 `stross_node` 类型（kernel 根部继续重导出，路径不变）；
- `Kernel::upsert_node<N: Node>`：trait 是内核接纳任意节点形态的**抽象面**
  （§2.2 节点注册表的真实消费点）。

### 10.5 插件挂载表（stross-kernel，复合键）

```rust
// stross-proto ids：端点插件全局限位（层级进地址；不进 wire）
pub struct EndpointRef { pub owner: NodeId, pub endpoint: EndpointId }

struct EndpointRegistry {
    endpoints: HashMap<EndpointRef, EndpointEntry>, // 复合键：宿主 + 插件
    file_sources: HashMap<EndpointRef, FileSource>,
}
```

- 读路径：`endpoint_entry(&EndpointRef)` / `manifest(&EndpointRef)` /
  `resolve_strategy(node, endpoint, …)`（内部构造 `EndpointRef`）——
  **node_id 不再被忽略**；
- 自节点便捷方法（`manifest` / `set_state` / `note_subscriber` /
  `on_subscribed` / `publish` / `unpublish` …）由 `UnifiedRegistry` 以
  `owner = self_node` 内部定位（调用方签名不变）；`register_remote_directory`
  以 `owner = 目录节点` 登记远端存根；
- `SubscribeService::resolve` 的 `delivery` 查表改 `(node, endpoint)` 限定
  （修复「远端 `screen:0` 拿到本机 delivery」的遮蔽）。

### 10.6 壳层消债（relay client + 可见性）

```rust
// stross-kernel relay/client.rs：服务对象取代自由函数
pub struct RelayClient { timeout: Duration }
impl RelayClient {
    pub fn new(timeout: Duration) -> Self;
    pub async fn probe_base(&self, base: &str) -> bool;
    pub async fn info(&self, host: &str, port: u16) -> Result<InfoResp>;
    pub async fn streams(&self, host: &str, port: u16) -> Result<Vec<StreamInfo>>;
}
```

- GUI `probe_relay` / CLI `adb status` 改调 `RelayClient`（✅ V3 已落地）；
- `verify_share_token` 收敛为模块自由函数（**校验单一真源**——数据面校验器
  `KernelTokenValidator` 复用它，消除两份重复校验逻辑；✅ V3）；`token_validator`
  保留 `pub` + `#[doc(hidden)]`（**数据面接线原语**：集成测试的独立接线场景
  需要公开构造路径，常规路径经 `attach_data_plane` 内部注入，壳层不直调）。

### 10.7 迁移映射（v3.1）

| 现状 | v3.1 处置 | 状态 |
|---|---|---|
| `stross-node::Node::endpoints()` | 删除（插件清单 = 注册表投影） | ✅ V1 |
| `kernel/graph.rs` NodeInfo/NodeRole/TransportAddr（与 stross-node 重复） | 删除 → 统一 stross-node；`impl Node for NodeInfo`；`upsert_node<N: Node>` 泛型化 | ✅ V1 |
| `EndpointRegistry` 键 `EndpointId` + `EndpointEntry.owner` | 键 `EndpointRef{owner, endpoint}`（proto ids，不进 wire），`owner` 字段删；读路径节点限定 | ✅ V2 |
| `resolve_strategy`/`stream_profile`/`resolve` 忽略 node_id | 按 `(node, endpoint)` 复合键精确取（修复跨节点遮蔽；新增回归测试） | ✅ V2 |
| 壳层直调 `relay::client::{probe_base,info,streams}` | `RelayClient` 服务对象（kernel 内部 `get_json`/`post_json` 保留；`stream_watchers` 自由函数删除——无消费者） | ✅ V3 |
| `verify_share_token`（pub，测试消费）+ 数据面校验器重复实现 | 收敛为模块自由函数单一真源，`KernelTokenValidator` 复用 | ✅ V3 |
| `token_validator` pub | `#[doc(hidden)]` 数据面接线原语（集成测试独立接线场景保留公开构造路径） | ✅ V3 |
| framework-v3.md 无 v3.1 | 本 §10 | ✅ 本文件 |

### 10.8 实施阶段（v3.1）

| 阶段 | 内容 | 验收 |
|---|---|---|
| V1 | stross-node 激活 + graph.rs 去重（Node trait 去 endpoints()；`impl Node for NodeInfo`；`upsert_node` 泛型化；kernel/壳层引用修齐） | ✅ cargo check/clippy 全绿 |
| V2 | 插件挂载表复合键（proto `EndpointRef`；registry 复合键；读路径节点限定；遮蔽修复 + 回归测试） | ✅ cargo test 全绿（44 套件） |
| V3 | 壳层消债（`RelayClient` 服务对象 + 壳层两处调用；`verify_share_token` 收敛自由函数单一真源 + `KernelTokenValidator` 复用；`token_validator` `#[doc(hidden)]`） | ✅ clippy 零告警 + kernel 109 测试全绿 |
| V4 | 文档 + 全量回归 + 大提交（AGENTS.md/dev-playbook 同步；full check） | ⏳ 全绿 + 单一大提交 |
