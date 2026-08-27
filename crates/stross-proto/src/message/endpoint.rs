//! 端点框架（节点 → 设备 → 端点）：设备 / 端点模型与 mDNS 摘要。
//!
//! 设计规格：docs/endpoint-model.md。
//!
//! * **设备**：节点上持久存在的能力实体（摄像头 / 麦克风 / 屏幕……），
//!   与订阅与否无关；
//! * **端点**：设备**被公开后**形成的订阅入口实例，携带公开者声明的协议
//!   （`transports` 优先序）、可见性、delivery 与运行状态。P1 为一设备一端点
//!   （1:1，`endpoint_id == device_id`）。
//!
//! 协议、可见性、delivery 全部由**公开者**决定；订阅方只在 `transports` 列表
//! 内协商/降级（复用 `Offer`/`Answer`）。

use serde::{Deserialize, Serialize};

use super::ids::{CodecId, MediaKind, TransportId};

/// 节点上的持久能力实体。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    /// 节点内稳定 id（"mic:builtin" / "screen:0" / "camera:front"）。
    pub device_id: String,
    pub kind: MediaKind,
    /// 用户可见名（"内置麦克风"）。
    pub name: String,
    /// 是否随节点常驻（静态枚举）。
    pub builtin: bool,
}

/// mDNS 摘要层（L1）：只带 id/kind/name/是否已公开，绝无协议、可见性等详情
/// （详情走 L2 `GET /api/endpoints` 拉取）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceSummary {
    pub device_id: String,
    pub kind: MediaKind,
    pub name: String,
    pub published: bool,
}

impl DeviceSummary {
    /// 由完整设备生成摘要。
    pub fn from_device(device: &DeviceInfo, published: bool) -> Self {
        Self {
            device_id: device.device_id.clone(),
            kind: device.kind,
            name: device.name.clone(),
            published,
        }
    }
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
    /// 已公开但无订阅（不采集）。
    Idle,
    /// 有订阅（正在推流）。
    Active,
    /// 暂停（手动挂起）。
    Suspended,
}

/// 端点清单：公开方协议 / 可见性 / delivery / 运行状态的唯一来源。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointManifest {
    /// 全局端点名（P1 1:1 下 == device_id；多端点版本解耦）。
    pub endpoint_id: String,
    pub device: DeviceInfo,
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

    #[test]
    fn device_summary_from_device() {
        let d = DeviceInfo {
            device_id: "mic:builtin".into(),
            kind: MediaKind::Mic,
            name: "麦克风".into(),
            builtin: true,
        };
        let s = DeviceSummary::from_device(&d, true);
        assert_eq!(s.device_id, "mic:builtin");
        assert!(s.published);
        assert_eq!(s.kind, MediaKind::Mic);
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
    fn manifest_roundtrip() {
        let m = EndpointManifest {
            endpoint_id: "mic:builtin".into(),
            device: DeviceInfo {
                device_id: "mic:builtin".into(),
                kind: MediaKind::Mic,
                name: "麦克风".into(),
                builtin: true,
            },
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
        };
        let text = serde_json::to_string(&m).unwrap();
        assert!(
            text.contains("\"endpointId\":\"mic:builtin\""),
            "wire: {text}"
        );
        let back: EndpointManifest = serde_json::from_str(&text).unwrap();
        assert_eq!(m, back);
    }
}
