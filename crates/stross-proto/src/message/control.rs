//! 控制消息（JSON 文本帧）。
//!
//! 协议 v2 在原有会话控制（Hello/Bye/Welcome/Ready/Error/Info）基础上，
//! 增加**能力协商**与**路由控制**（见 docs/plugin-architecture.md §5.2）：
//! 推流端/观看端上报能力（`Capabilities`），会话建立时协商传输与编解码
//! （`Offer`/`Answer`），会话存续期间可动态改道（`Route`）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::capability::{CapabilityDescriptor, RoutePath, SessionEventKind, TransportOffer};
use super::ids::{CodecId, ReliabilityProfile};
use super::stream::{StreamInfo, TrackInfo};

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
        /// 一次性接入凭证（跨设备推流用，见 [`super::token::ShareToken`]；
        /// 本机推流 / 旧端为 `None`）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        share_token: Option<String>,
    },
    /// 推流端结束。
    Bye,
    /// 观看端请求观看一个流（SRT/QUIC watch 的首条控制消息；
    /// WS 观看由 URL 查询参数声明，无需此消息）。
    Watch { stream_id: String },
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ids::{CapabilityKind, MediaKind, TransportId};
    use crate::message::token::ShareToken;

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
            share_token: None,
        };
        let text = msg.to_text();
        let back = ControlMessage::from_text(&text).unwrap();
        assert_eq!(msg, back);
        assert!(text.contains("\"type\":\"hello\""));
        // 无 token 时不应序列化 shareToken 字段（向后兼容：旧推流端 wire 不变）
        assert!(!text.contains("shareToken"), "text: {text}");
    }

    #[test]
    fn hello_with_share_token_roundtrip() {
        let token = ShareToken {
            v: ShareToken::VERSION,
            stream_id: "sess-1".into(),
            pin: "483920".into(),
            expires_at: 1_800_000_000,
            media: vec![MediaKind::Mic],
        };
        let msg = ControlMessage::Hello {
            stream_id: "sess-1".into(),
            title: "手机麦克风".into(),
            video: None,
            audio: Some(TrackInfo {
                codec: CodecId::Aac,
                width: None,
                height: None,
                fps: None,
                sample_rate: Some(48000),
                channels: Some(1),
            }),
            share_token: Some(token.to_token_string()),
        };
        let text = msg.to_text();
        // 注：internally-tagged enum（tag="type"）的字段名保持 snake_case
        // （与既有 stream_id 一致）；share_token 是嵌入的 ShareToken JSON 字符串，
        // 其内部为 camelCase（streamId）
        assert!(text.contains("\"share_token\""), "text: {text}");
        assert!(
            text.contains("\\\"streamId\\\":\\\"sess-1\\\""),
            "text: {text}"
        );
        let back = ControlMessage::from_text(&text).unwrap();
        assert_eq!(msg, back);
        // 旧客户端（无 share_token 字段）发来的 Hello 也应能解析
        let legacy = r#"{"type":"hello","stream_id":"s1","title":"t"}"#;
        match ControlMessage::from_text(legacy).unwrap() {
            ControlMessage::Hello { share_token, .. } => assert_eq!(share_token, None),
            other => panic!("期望 Hello，得到 {other:?}"),
        }
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
    fn watch_roundtrip() {
        let msg = ControlMessage::Watch {
            stream_id: "abc".into(),
        };
        let text = msg.to_text();
        assert!(text.contains("\"type\":\"watch\""), "text: {text}");
        assert_eq!(ControlMessage::from_text(&text).unwrap(), msg);
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
}
