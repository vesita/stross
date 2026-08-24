//! 控制消息（JSON 文本帧）。
//!
//! 协议 v2 在原有会话控制（Hello/Bye/Welcome/Ready/Error/Info）基础上，
//! 增加**能力协商**与**路由控制**（见 docs/plugin-architecture.md §5.2）：
//! 推流端/观看端上报能力（`Capabilities`），会话建立时协商传输与编解码
//! （`Offer`/`Answer`），会话存续期间可动态改道（`Route`）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 传输标识（有限集合）。
///
/// 用枚举而非字符串，让编译器在匹配/比较时穷尽检查（代码规范）；
/// `rename_all = "lowercase"` 保证线上 JSON 与 mDNS TXT 格式与字符串时代一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportId {
    /// WebSocket（TCP，无损）。
    Ws,
    /// WebRTC data channel（UDP，有损低延迟）。
    WebRtc,
    /// SRT（ARQ + 时延预算，自适应）。
    Srt,
    /// QUIC（多路复用，无损）。
    Quic,
    /// 内存传输（测试 / 示例用）。
    Memory,
}

/// 编解码标识（有限集合，可扩展）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodecId {
    H264,
    Aac,
    Opus,
    /// AV1（预留；传输/编码器支持后启用）。
    Av1,
}

/// 传输可靠性契约（设计文档 §4.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ReliabilityProfile {
    /// TCP-like：控制消息、输入注入、剪贴板 —— 全序不丢。
    Lossless,
    /// UDP-like：媒体帧 —— 允许丢帧，靠关键帧对齐自愈。
    #[default]
    Lossy,
    /// SRT-like：ARQ + 时延预算，超时则丢。
    Adaptive,
}

/// 能力种类：采集（Source）或 接收/注入（Sink）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CapabilityKind {
    Source,
    Sink,
}

/// 媒体能力类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MediaKind {
    Screen,
    Window,
    Camera,
    Mic,
    SystemAudio,
    Input,
    Clipboard,
}

/// 能力描述（Source 与 Sink 统一；mDNS TXT / 协商消息共用）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityDescriptor {
    pub kind: CapabilityKind,
    pub media: Vec<MediaKind>,
    /// 支持的编解码器（[`CodecId`]）。
    pub codecs: Vec<CodecId>,
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

/// 单条轨道信息（hello / 流信息用）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TrackInfo {
    pub codec: CodecId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<u8>,
}

/// 一条流的公开信息（REST `/api/streams` 与 ws 广播共用）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StreamInfo {
    pub stream_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<TrackInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<TrackInfo>,
    /// Unix 时间戳（秒）。
    pub started_at: u64,
    /// 当前观看者数量。
    pub watchers: u32,
}

/// 控制消息。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ControlMessage {
    /// 推流端声明开始推流。
    Hello {
        stream_id: String,
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        video: Option<TrackInfo>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio: Option<TrackInfo>,
    },
    /// 推流端结束。
    Bye,
    /// 中继端确认。
    Welcome { stream_id: String },
    /// 观看端就绪，可以开始收帧。
    Ready { stream_id: String },
    /// 错误。
    Error { message: String },
    /// 流列表（备用，目前主要走 REST）。
    Info { streams: Vec<StreamInfo> },
    /// 能力上报（握手后，供传输/编解码协商）。
    Capabilities { caps: Vec<CapabilityDescriptor> },
    /// 传输/编解码协商提议。
    Offer {
        session_id: String,
        transports: Vec<TransportOffer>,
        codecs: Vec<CodecId>,
        profile: ReliabilityProfile,
    },
    /// 协商应答。
    Answer {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transport: Option<TransportOffer>,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// 控制传输方向（会话存续期间动态改道）。
    Route { session_id: String, path: RoutePath },
    /// 会话事件广播。
    SessionEvent {
        session_id: String,
        event: SessionEventKind,
    },
}

impl ControlMessage {
    pub fn to_text(&self) -> String {
        serde_json::to_string(self).expect("ControlMessage 序列化不应失败")
    }

    pub fn from_text(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    pub fn from_value(v: Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(v)
    }
}

/// 设备角色（发现广播 F1.2 用；有限集合）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoleId {
    /// 可作源（推流）。
    Sender,
    /// 可作汇（接收播放）。
    Viewer,
    /// 中继（转发数据面）。
    Relay,
    /// 控制者（控制面；D7 远程控制阶段开放）。
    Controller,
}

/// mDNS TXT 中承载 [`DiscoveryInfo`] 的固定 key。
pub const TXT_KEY_DISCOVERY: &str = "stross";

/// 发现能力引导载荷（需求 F1.2；设计见 docs/requirements.md D7 旁注）。
///
/// 整个描述序列化进**单个 TXT key `stross`**（JSON）：注册侧 `to_txt` 一键编码、
/// 浏览侧 `from_txt` 一键解码——**新增字段零维护**，枚举经 serde 保持 wire
/// 格式稳定（lowercase，与旧逗号串时代一致）。设备名同时出现在 mDNS 实例名中
/// （第三方浏览器可读）；不再逐 key 手工拼 `name/roles/transports`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryInfo {
    /// 描述版本（自描述演进；当前 [`DiscoveryInfo::VERSION`]）。
    pub v: u8,
    /// 设备名。
    pub name: String,
    /// 角色。
    pub roles: Vec<RoleId>,
    /// 可共享的媒体类型。
    pub media: Vec<MediaKind>,
    /// 支持的传输。
    pub transports: Vec<TransportId>,
    /// 支持的编解码。
    pub codecs: Vec<CodecId>,
}

impl DiscoveryInfo {
    /// 当前描述版本。
    pub const VERSION: u8 = 1;

    /// 编码为 mDNS TXT 条目（单 key）。
    pub fn to_txt(&self) -> Vec<(String, String)> {
        vec![(
            TXT_KEY_DISCOVERY.to_string(),
            serde_json::to_string(self).expect("DiscoveryInfo 序列化不应失败"),
        )]
    }

    /// 从 mDNS TXT 条目解码；缺失 / 非法返回 `None`（调用方回退默认值）。
    pub fn from_txt(txt: &[(String, String)]) -> Option<Self> {
        let json = txt.iter().find(|(k, _)| k == TXT_KEY_DISCOVERY)?.1.as_str();
        serde_json::from_str(json).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_roundtrip() {
        let msg = ControlMessage::Hello {
            stream_id: "abc".into(),
            title: "测试".into(),
            video: Some(TrackInfo {
                codec: CodecId::H264,
                width: Some(1920),
                height: Some(1080),
                fps: Some(30),
                sample_rate: None,
                channels: None,
            }),
            audio: Some(TrackInfo {
                codec: CodecId::Aac,
                width: None,
                height: None,
                fps: None,
                sample_rate: Some(48000),
                channels: Some(2),
            }),
        };
        let text = msg.to_text();
        let back = ControlMessage::from_text(&text).unwrap();
        assert_eq!(msg, back);
        assert!(text.contains("\"type\":\"hello\""));
    }

    #[test]
    fn bye_roundtrip() {
        let text = ControlMessage::Bye.to_text();
        assert_eq!(
            ControlMessage::from_text(&text).unwrap(),
            ControlMessage::Bye
        );
    }

    #[test]
    fn capabilities_roundtrip() {
        let msg = ControlMessage::Capabilities {
            caps: vec![CapabilityDescriptor {
                kind: CapabilityKind::Source,
                media: vec![MediaKind::Screen, MediaKind::Mic],
                codecs: vec![CodecId::H264, CodecId::Aac],
                transports: vec![TransportId::Ws],
                max_width: Some(1920),
                max_height: Some(1080),
                preferred_profile: ReliabilityProfile::Lossy,
            }],
        };
        let text = msg.to_text();
        assert!(text.contains("\"type\":\"capabilities\""));
        let back = ControlMessage::from_text(&text).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn offer_answer_roundtrip() {
        let offer = ControlMessage::Offer {
            session_id: "s1".into(),
            transports: vec![TransportOffer {
                transport: TransportId::Ws,
                addr: "ws://127.0.0.1:8777/ws/push".into(),
                profile: ReliabilityProfile::Lossless,
            }],
            codecs: vec![CodecId::H264],
            profile: ReliabilityProfile::Lossy,
        };
        let back: ControlMessage = serde_json::from_str(&offer.to_text()).unwrap();
        assert_eq!(offer, back);

        let answer = ControlMessage::Answer {
            session_id: "s1".into(),
            transport: Some(TransportOffer {
                transport: TransportId::Ws,
                addr: "ws://127.0.0.1:8777/ws/watch".into(),
                profile: ReliabilityProfile::Lossless,
            }),
            ok: true,
            reason: None,
        };
        let back: ControlMessage = serde_json::from_str(&answer.to_text()).unwrap();
        assert_eq!(answer, back);
    }

    #[test]
    fn route_roundtrip() {
        let route = ControlMessage::Route {
            session_id: "s1".into(),
            path: RoutePath::ViaRelay {
                node: "relay-1".into(),
            },
        };
        let text = route.to_text();
        assert!(text.contains("\"kind\":\"viaRelay\""), "text: {text}");
        let back: ControlMessage = serde_json::from_str(&text).unwrap();
        assert_eq!(route, back);

        let mesh = ControlMessage::Route {
            session_id: "s1".into(),
            path: RoutePath::Mesh {
                nodes: vec!["a".into(), "b".into()],
            },
        };
        let back: ControlMessage = serde_json::from_str(&mesh.to_text()).unwrap();
        assert_eq!(mesh, back);
    }

    #[test]
    fn session_event_roundtrip() {
        let ev = ControlMessage::SessionEvent {
            session_id: "s1".into(),
            event: SessionEventKind::Started,
        };
        let back: ControlMessage = serde_json::from_str(&ev.to_text()).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn discovery_info_txt_roundtrip() {
        let info = DiscoveryInfo {
            v: DiscoveryInfo::VERSION,
            name: "卧室电脑".into(),
            roles: vec![RoleId::Relay, RoleId::Sender, RoleId::Viewer],
            media: vec![MediaKind::Screen, MediaKind::Mic],
            transports: vec![TransportId::Ws, TransportId::WebRtc],
            codecs: vec![CodecId::H264, CodecId::Aac],
        };
        let txt = info.to_txt();
        assert_eq!(txt.len(), 1, "单 key 承载全部能力");
        assert_eq!(txt[0].0, TXT_KEY_DISCOVERY);
        assert!(
            txt[0]
                .1
                .contains("\"roles\":[\"relay\",\"sender\",\"viewer\"]"),
            "wire: {}",
            txt[0].1
        );
        let back = DiscoveryInfo::from_txt(&txt).expect("roundtrip 解码");
        assert_eq!(info, back);
    }

    #[test]
    fn discovery_info_from_txt_tolerant() {
        // 缺失 key → None（调用方回退默认）
        assert_eq!(DiscoveryInfo::from_txt(&[]), None);
        assert_eq!(
            DiscoveryInfo::from_txt(&[("name".into(), "x".into())]),
            None
        );
        // 坏 JSON → None
        assert_eq!(
            DiscoveryInfo::from_txt(&[(TXT_KEY_DISCOVERY.into(), "{oops".into())]),
            None
        );
        // 未知枚举值 → 解码失败（枚举穷尽，未知值拒绝）
        assert_eq!(
            DiscoveryInfo::from_txt(&[(
                TXT_KEY_DISCOVERY.into(),
                r#"{"v":1,"name":"x","roles":["hacker"]}"#.into()
            )]),
            None
        );
    }
}
