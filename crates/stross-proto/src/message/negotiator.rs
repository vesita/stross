//! 凭证协商 / 订阅握手线协议（docs/endpoint-model.md §5）。
//!
//! 端到端消费者：`ShareNegotiator` 服务端（stross-app，axum）、订阅方客户端
//! （stross-app `subscriber` 模块）、CLI 与 Tauri 前端（经命令调库接口）。
//! 类型收敛在此处 = 线格式单一真源：任何一侧改字段，Rust 侧编译即报错，
//! 序列化由 `#[serde]` 保持与旧实现逐字节一致（旧字段名 / skip 语义未动）。

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::endpoint::{Delivery, EndpointManifest};
use super::ids::{MediaKind, PickRule, ReliabilityProfile, TransportId};

/// 一次性接入凭证视图（接收端签发后展示；订阅握手授予的 flatten 载荷）。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ShareTokenView {
    /// ShareToken JSON 字符串（原样粘贴给推流端 / 出站推流出示）。
    pub token: String,
    /// 接收端签发的会话 id（= 接收时的流 id）。
    pub stream_id: String,
    /// 一次性 PIN（展示用；服务端签发表为准）。
    pub pin: String,
    /// 过期时间（Unix 秒）。
    pub expires_at: u64,
}

/// 中继地址（pull 模式：订阅方连公开方中继；ws 必填，srt/quic 可缺）。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RelayAddr {
    pub ws_port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub srt_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quic_port: Option<u16>,
}

/// 设备申请凭证的请求（订阅握手）。
///
/// 端点语义（`endpoint_id` 非空 = 订阅某端点）：`media` 可为空，由端点推断；
/// 旧语义（`endpoint_id` 为空 = 接收方签发）与现状逐字节兼容。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ShareRequest {
    pub device_id: String,
    pub device_name: String,
    /// 订阅目标端点（端点框架，docs/endpoint-model.md §5）。
    #[serde(default)]
    pub endpoint_id: Option<String>,
    /// 订阅方期望的 delivery（端点声明 `Both` 时生效；其余以端点声明为准）。
    #[serde(default)]
    pub delivery_mode: Option<Delivery>,
    /// push 模式下订阅方自己的中继 HTTP 基址（`ws://ip:port`；公开方凭凭证
    /// 出站推送的目标）。
    #[serde(default)]
    pub relay_addr: Option<String>,
    /// push 模式下订阅方**自签**的一次性接入凭证（docs/endpoint-model.md §5：
    /// 凭证校验器挂在订阅方内核，公开方签发的凭证在订阅方中继校验不过）。
    #[serde(default)]
    pub share_token: Option<String>,
    /// 本次申请的媒体（有限集合；端点语义下可为空 = 由端点推断）。
    pub media: Vec<MediaKind>,
}

/// 签发结果（成功返回给申请方）。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ShareGrant {
    #[serde(flatten)]
    pub view: ShareTokenView,
    /// 是否因设备受信任而自动签发（未人工确认）。
    pub trusted: bool,
    /// 公开方拍板后的 delivery（端点语义；旧语义为 `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<Delivery>,
    /// 公开方接受的传输列表（按公开者声明的优先序；订阅方据此选择/降级）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transports: Option<Vec<TransportId>>,
    /// 协商定稿的传输层可靠性档案（端点语义；通信模式 v2，docs/comm-mode-v2.md §3）。
    /// `None` = 旧语义（无端点）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport_profile: Option<ReliabilityProfile>,
    /// 协商定稿的 pick 规则（端点语义）；订阅方据此装载对应解读模块。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pick_rule: Option<PickRule>,
    /// pull 模式：公开方中继地址；push 模式为 `None`（公开方凭凭证出站）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<RelayAddr>,
}

/// 目录节点摘要（`GET /api/endpoints` 响应的 `node` 字段）。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct EndpointNode {
    pub device_id: String,
    pub device_name: String,
}

/// L2 目录响应（`GET /api/endpoints`）：节点 + 端点清单（单层端点模型）。
/// Private 端点与不可挂载端点在服务端已过滤（§9），此处为公开快照。
/// L1 摘要 `EndpointSummary` 用于 mDNS，不在此响应。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct EndpointDir {
    pub node: EndpointNode,
    pub endpoints: Vec<EndpointManifest>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{MediaKind, Visibility};

    #[test]
    fn share_request_roundtrip_camel_case() {
        let req = ShareRequest {
            device_id: "dev-a".into(),
            device_name: "电脑".into(),
            endpoint_id: Some("mic:builtin".into()),
            delivery_mode: Some(Delivery::Push),
            relay_addr: Some("ws://192.168.1.5:41355".into()),
            share_token: Some("sess-9:abc:123".into()),
            media: vec![MediaKind::File],
        };
        let json = serde_json::to_value(&req).unwrap();
        // 端点语义字段：camelCase，与旧实现逐字一致
        assert_eq!(json["deviceId"], "dev-a");
        assert_eq!(json["endpointId"], "mic:builtin");
        assert_eq!(json["deliveryMode"], "push");
        assert_eq!(json["relayAddr"], "ws://192.168.1.5:41355");
        assert_eq!(json["shareToken"], "sess-9:abc:123");
        assert_eq!(json["media"][0], "file");
        let back: ShareRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back.device_id, "dev-a");
        assert_eq!(back.delivery_mode, Some(Delivery::Push));
    }

    #[test]
    fn share_grant_flattens_view() {
        let g = ShareGrant {
            view: ShareTokenView {
                token: "tok".into(),
                stream_id: "sess-1".into(),
                pin: "1234".into(),
                expires_at: 99,
            },
            trusted: true,
            delivery: Some(Delivery::Pull),
            transports: None,
            transport_profile: Some(ReliabilityProfile::Lossy),
            pick_rule: Some(PickRule::Realtime),
            relay: Some(RelayAddr {
                ws_port: 18777,
                srt_port: Some(33462),
                quic_port: None,
            }),
        };
        let json = serde_json::to_value(&g).unwrap();
        // flatten：token/streamId/pin/expiresAt 平铺在授予顶层
        assert_eq!(json["streamId"], "sess-1");
        assert_eq!(json["token"], "tok");
        assert_eq!(json["trusted"], true);
        assert_eq!(json["delivery"], "pull");
        assert_eq!(json["transportProfile"], "lossy");
        assert_eq!(json["pickRule"], "realtime");
        assert_eq!(json["relay"]["wsPort"], 18777);
        assert_eq!(json["relay"]["srtPort"], 33462);
        assert!(json["relay"].get("quicPort").is_none());
        let back: ShareGrant = serde_json::from_value(json).unwrap();
        assert_eq!(back.view.stream_id, "sess-1");
    }

    #[test]
    fn endpoint_dir_roundtrip() {
        let dir = EndpointDir {
            node: EndpointNode {
                device_id: "node-1".into(),
                device_name: "电脑".into(),
            },
            endpoints: vec![],
        };
        let json = serde_json::to_value(&dir).unwrap();
        assert_eq!(json["node"]["deviceId"], "node-1");
        // 单层端点模型：目录不再携带独立 devices 数组
        assert!(json.get("devices").is_none());
        let back: EndpointDir = serde_json::from_value(json).unwrap();
        assert_eq!(back.node.device_name, "电脑");
        let _ = serde_json::to_value(Visibility::Public).unwrap();
    }
}
