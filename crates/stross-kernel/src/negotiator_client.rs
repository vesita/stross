//! 协商端点（18779）的**客户端**（订阅握手 / 申请凭证）。
//!
//! 分层（docs/layering-architecture.md）：协商端点的服务端在
//! [`crate::negotiator`]（stross-app 拥有该 API），客户端据此收敛同层的
//! `request_grant`——CLI 订阅（`subscriber.rs`）、GUI 命令（`request_share_token`）、
//! 未来任何订阅方都从这里走，禁止壳层再手写 HTTP 握手。

use std::time::Duration;

use crate::relay::client as relay_http;
use stross_proto::message::{ShareGrant, ShareRequest};

/// 订阅握手 / 目录拉取的协商端点超时（必须盖过 Confirm 挂起窗 60s）：
/// 首见 Confirm 端点要求人工确认，读超时必须比挂起窗长，否则首见订阅
/// 会被误报失败。
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(70);

/// 订阅握手：POST `/api/negotiator/request`，返回公开方授予（或错误）。
///
/// `host`/`port` 是对端协商端点；`req` 为 [`ShareRequest`]（含端点语义 /
/// 旧信任语义两形态，见 proto 定义）。
pub async fn request_grant(
    host: &str,
    port: u16,
    req: &ShareRequest,
) -> anyhow::Result<ShareGrant> {
    let url = format!("http://{host}:{port}/api/negotiator/request");
    relay_http::post_json(&url, req, HANDSHAKE_TIMEOUT).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use stross_proto::message::{MediaKind, RelayAddr};

    /// 握手请求体构造即 wire 指纹（camelCase 字段、端点语义可选字段）；
    /// 与 proto wire 单测互补——确保客户端发的正是服务端解析的。
    #[test]
    fn request_serialization_matches_wire() {
        let req = ShareRequest {
            device_id: "dev-b".into(),
            device_name: "手机".into(),
            endpoint_id: Some("file:notes.txt".into()),
            strategy_id: None,
            delivery_mode: Some(stross_proto::message::Delivery::Push),
            relay_addr: Some("ws://192.168.1.5:41355".into()),
            share_token: Some("sess-9:abc:123".into()),
            media: vec![MediaKind::File],
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["deviceId"], "dev-b");
        assert_eq!(json["endpointId"], "file:notes.txt");
        assert_eq!(json["deliveryMode"], "push");
        assert_eq!(json["relayAddr"], "ws://192.168.1.5:41355");
        assert_eq!(json["shareToken"], "sess-9:abc:123");
        // 空端点（旧信任语义）：可选字段序列化为 null / 缺省（服务端按缺省
        // 解析，逐字节与旧实现一致）
        let old = ShareRequest {
            device_id: "dev-b".into(),
            device_name: "手机".into(),
            endpoint_id: None,
            strategy_id: None,
            delivery_mode: None,
            relay_addr: None,
            share_token: None,
            media: vec![MediaKind::Mic],
        };
        let json = serde_json::to_value(&old).unwrap();
        assert!(json["endpointId"].is_null(), "系列：None 字段序列化为 null");
        assert!(json["deliveryMode"].is_null());
        assert_eq!(json["media"][0], "mic");
        let _ = RelayAddr {
            ws_port: 18777,
            srt_port: None,
            quic_port: None,
        };
    }
}
