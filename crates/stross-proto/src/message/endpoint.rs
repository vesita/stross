//! 端点框架（节点 → 端点 → 策略）：**三层注册**与 L1 摘要。
//!
//! 设计规格：docs/framework-v3.md（v2 演进，取代 v1 单层模型；
//! v1 已删除（历史见 git））。
//!
//! * **端点**：节点上可共享的能力实体（屏幕 / 麦克风 / 摄像头 / 系统声音 /
//!   文件……）。端点自维护「可挂载性」（`available`，load 探测结果）、失败
//!   原因（`last_error`）与通告状态（`published`）；
//! * **行为契约**（端点 ↔ 内核约定，非语言特性）：每个端点实现 `load`
//!   （探测自身可用性，能否被挂载成节点）与 `share`（订阅达成后启动共享
//!   推流）——**双向能力体**：端点既能被订阅（分享端 `share`）、也能主动
//!   订阅别人（订阅端 `subscribe`），方向挂载在端点层，节点只是容器；
//! * **策略**（第三层）：端点自主声明的策略组合 [`EndpointStrategy`]（序列化
//!   规则 + pick 规则），策略独立可寻址（[`StrategyId`]）；注册表只记录
//!   「这个数据包怎么处理」的两要素，订阅按 `(节点, 端点, 策略)` 精确取；
//! * **目标类型**（内核契约层，不进 wire）：端点分两类——确定目标（文件等，
//!   内容预先确定，一次推送）与实时目标（相机等，内容持续产生，持续推流）；
//!   两类的共性抽象为 [`Endpoint`] 契约（stross-kernel），差异经目标类型
//!   维度 + 各端点实现表达。

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::ids::{CodecId, MediaKind, NodeId, PickRule, ReliabilityProfile, TransportId};

/// mDNS 摘要层（L1）：只带 id/kind/name/是否可挂载/是否已通告，绝无协议、
/// 可见性等详情（详情走 L2 `GET /api/endpoints` 拉取）。
///
/// **方案 A（端点 id 强类型化）**：`endpoint_id` 是**纯数值子 id**（本机族内
/// 唯一），`kind` 独立枚举字段承载能力族——wire 无前缀冗余，根治
/// `sysaudio`/`systemAudio` 漂移。消费方重建内部 [`super::ids::EndpointId`]：
/// `EndpointId::new(s.kind, s.endpoint_id)`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct EndpointSummary {
    pub endpoint_id: u32,
    pub kind: MediaKind,
    pub name: String,
    /// load 探测结果：能否被挂载成节点（共享源可用）。
    pub available: bool,
    /// 是否已通告（目录可见、可订阅）。
    pub published: bool,
}

/// 端点可见性（决定**目录可见性 + 授予决策**两件事）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum Visibility {
    /// 任何人可订阅，免确认。
    Public,
    /// 首见人工确认；已信任节点自动（复用 TrustStore）。
    Confirm,
    /// 仅白名单节点（按节点 node_id；不出现在目录响应中）。
    Private { nodes: Vec<NodeId> },
}

impl Visibility {
    /// wire 字符串（camelCase；与 serde 序列化一致，单一真源）。
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Confirm => "confirm",
            Self::Private { .. } => "private",
        }
    }

    /// 从 wire 字符串解析（与 [`as_str`](Self::as_str) 互逆；未知值返回 `None`）。
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "public" => Some(Self::Public),
            "confirm" => Some(Self::Confirm),
            "private" => Some(Self::Private { nodes: Vec::new() }),
            _ => None,
        }
    }
}

/// 数据面连接方向（由公开者声明；订阅握手时定稿）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum Delivery {
    /// 订阅者连公开者中继（watch 路径）。
    Pull,
    /// 公开者凭凭证出站连订阅者中继（push 路径）。
    Push,
    /// 两种都可，订阅方可在握手中选一种。
    Both,
}

impl Delivery {
    crate::message::define_wire_strings! {
        Delivery:
            Pull => "pull",
            Push => "push",
            Both => "both",
    }
}

/// 公开者选择的传输协议（priority 升序 = 公开者偏好）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TransportPreference {
    pub transport: TransportId,
    pub priority: u8,
}

/// 策略 id：端点内**独立可寻址**（同一内容可有多种处理组合；
/// docs/framework-v3.md §2——订阅按 `(节点, 端点, 策略)` 精确取）。
/// 强类型枚举，具备零堆分配、穷尽匹配与编译期检查。
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    Default,
    ToSchema,
)]
#[serde(rename_all = "camelCase")]
pub enum StrategyId {
    /// 默认策略（直通）。
    #[default]
    Default,
    /// 直通策略。
    Passthrough,
    /// 大块分片策略。
    Chunked,
}

impl StrategyId {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Passthrough => "passthrough",
            Self::Chunked => "chunked",
        }
    }

    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "default" => Some(Self::Default),
            "passthrough" => Some(Self::Passthrough),
            "chunked" => Some(Self::Chunked),
            _ => None,
        }
    }
}

impl std::fmt::Display for StrategyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::str::FromStr for StrategyId {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_wire(s).ok_or_else(|| format!("未知的策略 id: {s}"))
    }
}

impl From<&str> for StrategyId {
    fn from(s: &str) -> Self {
        s.parse().unwrap_or(Self::Default)
    }
}

/// 序列化规则（SerializeRule）：数据 ↔ 管线格式的转换（装载/解装载，含分包）——
/// 端点自定，内核不碰编码细节（docs/framework-v3.md §0/§2）。
///
/// 与 [`PickRule`]（管线内怎么解读）正交：本枚举描述「怎么把数据转成管线
/// 格式」；**枚举（确定性，wire 可比对）**，端点实现按变体映射装载/解装载模块。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum SerializeRule {
    /// 直通（Passthrough）：帧即数据，不额外装载/分包。当前全部端点的默认。
    #[default]
    Passthrough,
    /// 分包（Chunked）：大数据块按管线分片装载/解装载（预留：文件大块、
    /// 协议升级等场景启用；当前端点均未声明）。
    Chunked,
}

/// 端点策略：注册表只记录「这个数据包怎么处理」的两要素——
/// 序列化规则（[`SerializeRule`]：怎么装载/解装载，含分包）+ pick 规则
/// （[`PickRule`]：管线里怎么解读，docs/framework-v3.md §3.0）。
///
/// 传输档案（[`ReliabilityProfile`]）**不进注册表**（端点声明、传输模块执行）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct EndpointStrategy {
    /// 策略 id（端点内独立可寻址；订阅方按它指明要哪个策略）。
    pub strategy_id: StrategyId,
    /// 序列化规则（装载/解装载，含分包）。
    pub serialize: SerializeRule,
    /// pick 规则（严格即时/严格顺序/无）。
    pub pick: PickRule,
}

impl EndpointStrategy {
    /// 默认策略 id（端点未声明多策略时的唯一策略；订阅方缺省按它取）。
    pub const DEFAULT_ID: StrategyId = StrategyId::Default;

    /// 直通序列化 + 指定 pick 规则的默认策略（当前唯一实现组合）。
    ///
    /// 全仓构造默认策略的**单一真源**（此前各处手写字面量重复，见
    /// docs/framework-v3.md §3）；新增序列化规则时在此扩展构造函数。
    pub fn passthrough(pick: PickRule) -> Self {
        Self {
            strategy_id: StrategyId::Default,
            serialize: SerializeRule::Passthrough,
            pick,
        }
    }
}

/// 订阅规格（订阅端点生成依据，docs/framework-v3.md §3）：
/// 从注册表取 `(节点, 端点, 策略)` → 策略组合 → 生成订阅端点。
///
/// 方向挂载在端点层（节点只是「拥有多个端点」的容器，不承载方向）；
/// 订阅端（[`Endpoint::subscribe`]）据此装载对应管线并处理订阅流/数据。
///
/// **方案 A（端点 id 强类型化）**：`endpoint_id` 是**数值子 id**，`kind` 独立
/// 枚举字段——订阅方收到目录后无需反解前缀即可重建
/// [`super::ids::EndpointId`]（`EndpointId::new(s.kind, s.endpoint_id)`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeSpec {
    /// 互联节点 id（对端 device_id；本机订阅本机时为本地节点 id）。
    pub node_id: super::ids::NodeId,
    /// 订阅目标端点能力族（与 `endpoint_id` 组合成内部 `EndpointId`）。
    pub kind: MediaKind,
    /// 对端端点数值子 id（"0" / "5"；与 `kind` 组合）。
    pub endpoint_id: u32,
    /// 选定的策略 id（注册表第三层；`None` = 取端点默认策略）。
    pub strategy_id: Option<StrategyId>,
    /// 策略组合（序列化规则 + pick 规则；注册表查得，订阅端点据此装载管线）。
    pub strategy: EndpointStrategy,
    /// 定稿后的数据面方向（订阅驱动定稿只走 Pull）。
    pub delivery: Delivery,
    /// 数据面流 id（pull = 公开方签发的会话）。
    pub stream_id: super::ids::StreamId,
    /// 订阅方连接公开方中继的 WS 基址（`ws://host:port`；pull 模式）。
    pub relay_url: Option<String>,
}

/// 端点运行状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum EndpointState {
    /// 已通告但无订阅（不采集/不推送）。
    Idle,
    /// 有订阅（正在共享：推流 / 传文件）。
    Active,
    /// 暂停（手动挂起）。
    Suspended,
}

impl EndpointState {
    crate::message::define_wire_strings! {
        EndpointState:
            Idle => "idle",
            Active => "active",
            Suspended => "suspended",
    }
}

/// 端点清单：公开方协议 / 可见性 / delivery / 挂载性 / 运行状态的唯一来源。
///
/// 单层端点模型：端点 = 节点上可共享的能力实体（原「设备」与「端点」合并），
/// `published` 表示是否已通告；未通告端点只在本机目录可见，不进对端目录/
/// mDNS 摘要的可订阅集。
///
/// **方案 A（端点 id 强类型化）**：`endpoint_id` 是**纯数值子 id**（本机族内
/// 唯一），`kind` 独立枚举字段承载能力族。重建内部 [`super::ids::EndpointId`]：
/// `EndpointId::new(m.kind, m.endpoint_id)`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct EndpointManifest {
    /// 端点数值子 id（本机族内唯一；与 `kind` 组合成 [`super::ids::EndpointId`]）。
    pub endpoint_id: u32,
    pub kind: MediaKind,
    /// 用户可见名（"内置麦克风"）。
    pub name: String,
    /// load 探测结果：能否被挂载成节点（共享源可用）。
    pub available: bool,
    /// load/share 失败原因（`available=false` 时展示给用户；对端目录可见但
    /// 不可订阅）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// 是否已通告（未通告 = 仅本机可见，不进目录/摘要可订阅集）。
    pub published: bool,
    pub visibility: Visibility,
    pub delivery: Delivery,
    /// 公开者选择的传输（按 priority 升序）。
    pub transports: Vec<TransportPreference>,
    /// 传输层可靠性档案（允许丢包/不允许丢包/自适应；端点在 `share()` 前
    /// 据此装载对应传输模块）。缺省 Lossy（媒体实时路径）。
    #[serde(default)]
    pub transport_profile: ReliabilityProfile,
    /// pick 规则（装载/解读语义：严格即时/严格顺序/无；发送侧装载逻辑与
    /// 接收侧解读模块共用，docs/framework-v3.md §3.0）。缺省 Realtime。
    ///
    /// **v2 演进**：pick 规则收敛进 [`EndpointStrategy`]（与序列化规则组合成
    /// 策略）；本平铺字段保留为**默认策略的协商摘要**（wire 兼容旧对端），
    /// 新消费方优先读 `strategies`。
    #[serde(default)]
    pub pick_rule: PickRule,
    /// 端点自主声明的策略组合（策略独立可寻址；订阅按 `(节点, 端点, 策略)`
    /// 精确取，docs/framework-v3.md §2）。缺省 = 由平铺
    /// `serialize(直通) + pick_rule` 推导的单默认策略。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub strategies: Vec<EndpointStrategy>,
    /// 该端点实际可用的编解码。
    pub codecs: Vec<CodecId>,
    pub state: EndpointState,
    /// 当前订阅数（= 关联会话 watchers/sinks）。
    pub subscribers: u32,
    /// 最近变更时刻（Unix 秒）。
    pub updated_at: u64,
}

impl EndpointManifest {
    /// 解析端点策略（**确定性单一真源**，docs/framework-v3.md §2）：
    /// 按订阅方选定的策略 id 精确取；`None` = 端点默认策略（首个）；清单无
    /// 策略列表时由平铺 `pick_rule` 推导直通 + pick 的默认策略（旧对端兼容）。
    ///
    /// 注册表（`kernel::endpoint`）、协商层（`negotiator`）与订阅编排共用本
    /// 方法——此前各层各自实现"按 id → 默认 → 推导"回退链，且注册表默认
    /// 用 HashMap `values().next()` 非确定性选取，本方法统一为「Vec 首个」
    /// 的确定性语义。
    pub fn strategy(&self, strategy_id: Option<&str>) -> EndpointStrategy {
        self.strategies
            .iter()
            .find(|s| Some(s.strategy_id.as_str()) == strategy_id)
            .copied()
            .or_else(|| self.strategies.first().copied())
            .unwrap_or_else(|| EndpointStrategy::passthrough(self.pick_rule))
    }
}

/// 文件端点元数据（docs/framework-v3.md §3）：作为文件流**首帧**（FLAG_CONFIG）
/// 的 JSON 载荷下发给接收方。路径只存在于公开方本地（`EndpointRegistry.file_sources`），
/// **绝不进入本结构 / 目录 / mDNS 摘要**。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileMeta {
    /// 接收方落盘用的文件名（仅文件名，不含路径）。
    pub name: String,
    /// 文件字节数（接收方校验完整性）。
    pub size: u64,
    /// SHA-256 十六进制（可选；P1 未计算，恒为 `None`，仅大小校验）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

impl FileMeta {
    /// 编码为首帧载荷。
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("FileMeta 序列化不应失败")
    }

    /// 从首帧载荷解析；非法返回 `None`（接收方拒绝该文件流）。
    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        serde_json::from_slice(buf).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> EndpointManifest {
        EndpointManifest {
            endpoint_id: 0,
            kind: MediaKind::Mic,
            name: "麦克风".into(),
            available: true,
            last_error: None,
            published: true,
            visibility: Visibility::Confirm,
            delivery: Delivery::Both,
            transports: vec![
                TransportPreference {
                    transport: TransportId::Quic,
                    priority: 0,
                },
                TransportPreference {
                    transport: TransportId::Ws,
                    priority: 1,
                },
            ],
            transport_profile: ReliabilityProfile::Lossy,
            pick_rule: PickRule::Realtime,
            strategies: vec![EndpointStrategy::passthrough(PickRule::Realtime)],
            codecs: vec![CodecId::Aac],
            state: EndpointState::Idle,
            subscribers: 0,
            updated_at: 1_800_000_000,
        }
    }

    #[test]
    fn endpoint_summary_wire() {
        let s = EndpointSummary {
            endpoint_id: 0,
            kind: MediaKind::Screen,
            name: "屏幕".into(),
            available: false,
            published: true,
        };
        let text = serde_json::to_string(&s).unwrap();
        assert!(text.contains("\"endpointId\":0"), "wire: {text}");
        assert!(text.contains("\"available\":false"), "wire: {text}");
        let back: EndpointSummary = serde_json::from_str(&text).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn visibility_wire_and_roundtrip() {
        for v in [
            Visibility::Public,
            Visibility::Confirm,
            Visibility::Private {
                nodes: vec![NodeId::from("node-a")],
            },
        ] {
            let text = serde_json::to_string(&v).unwrap();
            let back: Visibility = serde_json::from_str(&text).unwrap();
            assert_eq!(v, back, "wire: {text}");
        }
        // wire 格式稳定（camelCase；Private 为带载荷变体）
        assert_eq!(
            serde_json::to_string(&Visibility::Public).unwrap(),
            "\"public\""
        );
        assert_eq!(
            serde_json::to_string(&Visibility::Private {
                nodes: vec![NodeId::from("node-a")]
            })
            .unwrap(),
            format!(
                r#"{{"private":{{"nodes":["{}"]}}}}"#,
                NodeId::from("node-a")
            )
        );
    }

    #[test]
    fn wire_strings_roundtrip() {
        // as_str / from_wire 与 serde 序列化保持一致（单一真源不漂移）
        for v in [Visibility::Public, Visibility::Confirm] {
            assert_eq!(Visibility::from_wire(v.as_str()), Some(v.clone()));
            assert_eq!(
                serde_json::to_string(&v).unwrap(),
                format!("\"{}\"", v.as_str())
            );
        }
        // Private 的节点清单不在 wire 字符串里（解析出空清单，调用方补充）
        let v = Visibility::Private {
            nodes: vec![NodeId::from("node-a")],
        };
        assert_eq!(
            Visibility::from_wire(v.as_str()),
            Some(Visibility::Private { nodes: vec![] })
        );
        assert_eq!(Visibility::from_wire("internal"), None);

        for d in [Delivery::Pull, Delivery::Push, Delivery::Both] {
            assert_eq!(Delivery::from_wire(d.as_str()), Some(d));
            assert_eq!(
                serde_json::to_string(&d).unwrap(),
                format!("\"{}\"", d.as_str())
            );
        }
        assert_eq!(Delivery::from_wire("upstream"), None);

        for s in [
            EndpointState::Idle,
            EndpointState::Active,
            EndpointState::Suspended,
        ] {
            assert_eq!(
                serde_json::to_string(&s).unwrap(),
                format!("\"{}\"", s.as_str())
            );
        }
        // MediaKind 全变体与 serde 一致（camelCase）
        for k in [
            crate::message::ids::MediaKind::Screen,
            crate::message::ids::MediaKind::Window,
            crate::message::ids::MediaKind::Camera,
            crate::message::ids::MediaKind::Mic,
            crate::message::ids::MediaKind::SystemAudio,
            crate::message::ids::MediaKind::Input,
            crate::message::ids::MediaKind::Clipboard,
            crate::message::ids::MediaKind::File,
            crate::message::ids::MediaKind::Service,
        ] {
            assert_eq!(
                serde_json::to_string(&k).unwrap(),
                format!("\"{}\"", k.as_str())
            );
        }
    }

    #[test]
    fn file_meta_roundtrip() {
        let m = FileMeta {
            name: "notes.txt".into(),
            size: 42,
            sha256: None,
        };
        let back = FileMeta::from_bytes(&m.to_bytes()).unwrap();
        assert_eq!(m, back);
        // 载荷不带 sha256（未计算时字段省略）；wire 为 camelCase
        let text = String::from_utf8(m.to_bytes()).unwrap();
        assert!(text.contains("\"name\":\"notes.txt\""), "wire: {text}");
        assert!(text.contains("\"size\":42"), "wire: {text}");
        assert!(!text.contains("sha256"), "wire: {text}");
        // 非法载荷 → None
        assert!(FileMeta::from_bytes(b"{oops").is_none());
    }

    #[test]
    fn manifest_roundtrip_flat_single_layer() {
        let m = sample_manifest();
        let text = serde_json::to_string(&m).unwrap();
        // 单层端点模型：平铺 kind/name（无 device 嵌套）；endpointId 数值化
        assert!(text.contains("\"endpointId\":0"), "wire: {text}");
        assert!(text.contains("\"kind\":\"mic\""), "wire: {text}");
        assert!(
            !text.contains("\"device\""),
            "单层模型不应有 device 嵌套: {text}"
        );
        // 通信模式 v2 档案字段上 wire（传输档案 + pick 规则；camelCase）
        assert!(
            text.contains("\"transportProfile\":\"lossy\""),
            "传输档案应上 wire: {text}"
        );
        assert!(
            text.contains("\"pickRule\":\"realtime\""),
            "pick 规则应上 wire: {text}"
        );
        // v2 策略组合上 wire（三层注册表第三层：序列化规则 + pick 规则）
        assert!(
            text.contains("\"strategies\":[{\"strategyId\":\"default\",\"serialize\":\"passthrough\",\"pick\":\"realtime\"}]"),
            "策略组合应上 wire: {text}"
        );
        let back: EndpointManifest = serde_json::from_str(&text).unwrap();
        assert_eq!(m, back);
        assert_eq!(back.strategies.len(), 1);
        assert_eq!(back.strategies[0].strategy_id, StrategyId::Default);
        // 不可用端点的 last_error 上 wire
        let mut m2 = sample_manifest();
        m2.available = false;
        m2.last_error = Some("无图形会话".into());
        let text2 = serde_json::to_string(&m2).unwrap();
        assert!(
            text2.contains("\"lastError\":\"无图形会话\""),
            "wire: {text2}"
        );
        let back2: EndpointManifest = serde_json::from_str(&text2).unwrap();
        assert_eq!(m2, back2);
    }

    /// 旧 wire（无档案字段）反序列化：`#[serde(default)]` 回退到 Lossy/Realtime，
    /// 策略列表为空（消费方按平铺字段推导默认策略），不破坏旧对端 / 旧目录缓存
    /// 的兼容性。
    #[test]
    fn manifest_parses_without_profile_fields() {
        // 旧 wire（无档案字段）反序列化：`#[serde(default)]` 回退到 Lossy/Realtime，
        // 策略列表为空（消费方按平铺字段推导默认策略），不破坏旧对端 / 旧目录缓存
        // 的兼容性。注：endpointId 已按方案 A 数值化（旧字符串端点 id 不再可解析）。
        let old = r#"{"endpointId":0,"kind":"mic","name":"麦克风","available":true,"published":true,"visibility":"public","delivery":"pull","transports":[],"codecs":[],"state":"idle","subscribers":0,"updatedAt":1800000000}"#;
        let m: EndpointManifest = serde_json::from_str(old).unwrap();
        assert_eq!(m.endpoint_id, 0);
        assert_eq!(m.kind, MediaKind::Mic);
        assert_eq!(m.transport_profile, ReliabilityProfile::Lossy);
        assert_eq!(m.pick_rule, PickRule::Realtime);
        assert!(m.strategies.is_empty(), "旧 wire 无策略列表");
    }

    /// 新 wire（含 strategies）roundtrip：策略组合逐字节稳定（camelCase），
    /// 缺省策略 id 为 "default"。
    #[test]
    fn strategy_wire_roundtrip() {
        let s = EndpointStrategy::passthrough(PickRule::StrictOrdered);
        let text = serde_json::to_string(&s).unwrap();
        assert_eq!(
            text,
            r#"{"strategyId":"default","serialize":"passthrough","pick":"strictOrdered"}"#
        );
        let back: EndpointStrategy = serde_json::from_str(&text).unwrap();
        assert_eq!(s, back);
    }

    /// SubscribeSpec roundtrip：订阅端点生成的输入契约（camelCase；缺省
    /// strategy_id 允许省略——订阅方取端点默认策略）。
    #[test]
    fn subscribe_spec_wire_roundtrip() {
        let spec = SubscribeSpec {
            node_id: "node-phone".into(),
            kind: MediaKind::Screen,
            endpoint_id: 0,
            strategy_id: None,
            strategy: EndpointStrategy::passthrough(PickRule::Realtime),
            delivery: Delivery::Pull,
            stream_id: "sess-1".into(),
            relay_url: Some("ws://192.168.1.5:18777".into()),
        };
        let text = serde_json::to_string(&spec).unwrap();
        assert!(
            text.contains(&format!("\"nodeId\":\"{}\"", spec.node_id)),
            "wire: {text}"
        );
        assert!(text.contains("\"kind\":\"screen\""), "wire: {text}");
        assert!(text.contains("\"endpointId\":0"), "wire: {text}");
        assert!(text.contains("\"strategyId\":null"), "wire: {text}");
        assert!(
            text.contains("\"relayUrl\":\"ws://192.168.1.5:18777\""),
            "wire: {text}"
        );
        let back: SubscribeSpec = serde_json::from_str(&text).unwrap();
        assert_eq!(spec, back);
        assert_eq!(back.kind, MediaKind::Screen);
        assert_eq!(back.endpoint_id, 0);
        let _ = SerializeRule::default();
    }

    // Delivery / EndpointState 已由 define_wire_strings! 生成 as_str/from_wire，
    // 用一致性宏与 serde「单一真源」核验（新增变体漏改即崩测试）。
    crate::message::assert_wire_strings_consistent! {
        delivery_wire_matches_serde: Delivery;
        Delivery::Pull => "pull",
        Delivery::Push => "push",
        Delivery::Both => "both",
    }
    crate::message::assert_wire_strings_consistent! {
        endpoint_state_wire_matches_serde: EndpointState;
        EndpointState::Idle => "idle",
        EndpointState::Active => "active",
        EndpointState::Suspended => "suspended",
    }
}
