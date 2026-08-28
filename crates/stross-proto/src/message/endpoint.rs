//! 端点框架（节点 → 端点）：**单层端点模型**与 L1 摘要。
//!
//! 设计规格：docs/endpoint-model.md。
//!
//! * **端点**：节点上可共享的能力实体（屏幕 / 麦克风 / 摄像头 / 系统声音 /
//!   文件……）。端点自维护「可挂载性」（`available`，load 探测结果）、失败
//!   原因（`last_error`）与通告状态（`published`）；
//! * **行为契约**（端点 ↔ 内核约定，非语言特性）：每个端点实现 `load`
//!   （探测自身可用性，能否被挂载成节点）与 `share`（订阅达成后启动共享
//!   推流），内核不做类型分派；
//! * **目标类型**（内核契约层，不进 wire）：端点分两类——确定目标（文件等，
//!   内容预先确定，一次推送）与实时目标（相机等，内容持续产生，持续推流）；
//!   两类的共性抽象为 [`Endpoint`] 契约（stross-kernel），差异经目标类型
//!   维度 + 各端点实现表达。

use serde::{Deserialize, Serialize};

use super::ids::{CodecId, MediaKind, TransportId};

/// mDNS 摘要层（L1）：只带 id/kind/name/是否可挂载/是否已通告，绝无协议、
/// 可见性等详情（详情走 L2 `GET /api/endpoints` 拉取）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointSummary {
    pub endpoint_id: String,
    pub kind: MediaKind,
    pub name: String,
    /// load 探测结果：能否被挂载成节点（共享源可用）。
    pub available: bool,
    /// 是否已通告（目录可见、可订阅）。
    pub published: bool,
}

/// 端点可见性（决定**目录可见性 + 授予决策**两件事）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Visibility {
    /// 任何人可订阅，免确认。
    Public,
    /// 首见人工确认；已信任节点自动（复用 TrustStore）。
    Confirm,
    /// 仅白名单节点（按节点 device_id；不出现在目录响应中）。
    Private { nodes: Vec<String> },
}

impl Visibility {
    /// wire 字符串（camelCase；与 serde 序列化一致，单一真源）。
    pub const fn as_str(&self) -> &'static str {
        match self {
            Visibility::Public => "public",
            Visibility::Confirm => "confirm",
            Visibility::Private { .. } => "private",
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    /// wire 字符串（camelCase；与 serde 序列化一致，单一真源）。
    pub const fn as_str(&self) -> &'static str {
        match self {
            Delivery::Pull => "pull",
            Delivery::Push => "push",
            Delivery::Both => "both",
        }
    }

    /// 从 wire 字符串解析（与 [`as_str`](Self::as_str) 互逆；未知值返回 `None`）。
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "pull" => Some(Self::Pull),
            "push" => Some(Self::Push),
            "both" => Some(Self::Both),
            _ => None,
        }
    }
}

/// 公开者选择的传输协议（priority 升序 = 公开者偏好）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransportPreference {
    pub transport: TransportId,
    pub priority: u8,
}

/// 端点运行状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    /// wire 字符串（camelCase；与 serde 序列化一致，单一真源）。
    pub const fn as_str(&self) -> &'static str {
        match self {
            EndpointState::Idle => "idle",
            EndpointState::Active => "active",
            EndpointState::Suspended => "suspended",
        }
    }
}

/// 端点清单：公开方协议 / 可见性 / delivery / 挂载性 / 运行状态的唯一来源。
///
/// 单层端点模型：端点 = 节点上可共享的能力实体（原「设备」与「端点」合并），
/// `published` 表示是否已通告；未通告端点只在本机目录可见，不进对端目录/
/// mDNS 摘要的可订阅集。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointManifest {
    /// 节点内稳定 id（"screen:0" / "mic:builtin" / "file:notes.txt"）。
    pub endpoint_id: String,
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
    /// 该端点实际可用的编解码。
    pub codecs: Vec<CodecId>,
    pub state: EndpointState,
    /// 当前订阅数（= 关联会话 watchers/sinks）。
    pub subscribers: u32,
    /// 最近变更时刻（Unix 秒）。
    pub updated_at: u64,
}

/// 文件端点元数据（docs/endpoint-model.md §3.6）：作为文件流**首帧**（FLAG_CONFIG）
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
            endpoint_id: "mic:builtin".into(),
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
            codecs: vec![CodecId::Aac],
            state: EndpointState::Idle,
            subscribers: 0,
            updated_at: 1_800_000_000,
        }
    }

    #[test]
    fn endpoint_summary_wire() {
        let s = EndpointSummary {
            endpoint_id: "screen:0".into(),
            kind: MediaKind::Screen,
            name: "屏幕".into(),
            available: false,
            published: true,
        };
        let text = serde_json::to_string(&s).unwrap();
        assert!(text.contains("\"endpointId\":\"screen:0\""), "wire: {text}");
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
                nodes: vec!["dev-a".into()],
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
                nodes: vec!["dev-a".into()]
            })
            .unwrap(),
            r#"{"private":{"nodes":["dev-a"]}}"#
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
            nodes: vec!["dev-a".into()],
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
        // 单层端点模型：平铺 kind/name（无 device 嵌套）
        assert!(
            text.contains("\"endpointId\":\"mic:builtin\""),
            "wire: {text}"
        );
        assert!(text.contains("\"kind\":\"mic\""), "wire: {text}");
        assert!(
            !text.contains("\"device\""),
            "单层模型不应有 device 嵌套: {text}"
        );
        let back: EndpointManifest = serde_json::from_str(&text).unwrap();
        assert_eq!(m, back);
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
}
