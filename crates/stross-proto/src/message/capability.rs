//! 能力描述与协商 / 路由控制类型（Source 与 Sink 统一）。

use serde::{Deserialize, Serialize};

use super::ids::{CapabilityKind, MediaKind, ReliabilityProfile, TransportId};

/// 能力描述（Source 与 Sink 统一；mDNS TXT / 协商消息共用）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityDescriptor {
    pub kind: CapabilityKind,
    pub media: Vec<MediaKind>,
    /// 支持的编解码器（[`CodecId`](super::ids::CodecId)）。
    pub codecs: Vec<super::ids::CodecId>,
    /// 支持的传输（[`TransportId`]）。
    pub transports: Vec<TransportId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_height: Option<u32>,
    /// 该能力期望的传输可靠性。
    pub preferred_profile: ReliabilityProfile,
}

impl CapabilityDescriptor {
    /// 未知能力（默认实现用）。
    pub fn unknown() -> Self {
        Self {
            kind: CapabilityKind::Source,
            media: Vec::new(),
            codecs: Vec::new(),
            transports: Vec::new(),
            max_width: None,
            max_height: None,
            preferred_profile: ReliabilityProfile::Lossy,
        }
    }
}

/// 传输协商候选（Offer 中的一项）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransportOffer {
    pub transport: TransportId,
    pub addr: String,
    pub profile: ReliabilityProfile,
}

/// 路由路径（「控制传输方向」的直接体现）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RoutePath {
    /// 直连（能力允许时优先）。
    Direct { node: String },
    /// 经中继兜底。
    ViaRelay { node: String },
    /// 组播 / 多目标。
    Mesh { nodes: Vec<String> },
}

/// 会话事件种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionEventKind {
    Started,
    Ended,
    Lost,
}
