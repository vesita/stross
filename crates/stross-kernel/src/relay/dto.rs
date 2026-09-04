//! 中继 REST API 的数据传输对象（DTO）。
//!
//! 线格式契约：`POST /api/*` 的请求体与响应体（`GET /api/*` 的响应体）。
//! 与 [`super::api`] 的 `#[utoipa::path]` 宏一一对应（body = 本文件类型）；
//! 跨类型复用复用 [`stross_proto::message::StreamInfo`] /
//! [`super::PeerInfo`]（已带 `ToSchema`）。

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use stross_proto::message::{StreamId, StreamInfo};

/// 错误响应体（统一 `{ "error": "..." }`）。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiError {
    pub error: String,
}

/// 中继入口信息（各传输端口；前端据此构造 `srt://` / `quic://` 拨号地址）。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RelayInfoResp {
    /// HTTP/WS 端口。
    pub port: u16,
    /// SRT 推流/观看端口（随机分配）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub srt_port: Option<u16>,
    /// QUIC 推流/观看端口（随机分配）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quic_port: Option<u16>,
}

/// 请求建立代理流（`POST /api/proxy`）。
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProxyReq {
    /// 上游中继基址（`ws://host:port`；`srt://` / `quic://` 亦可）。
    pub upstream: String,
    /// 上游流 id。
    pub stream_id: StreamId,
    /// 上游流信息（可选；前端自动发现时已持有，透传避免再向上游查询）。
    #[serde(default)]
    pub info: Option<StreamInfo>,
}

/// 建立代理流响应（`POST /api/proxy`）。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProxyStartResp {
    pub stream_id: StreamId,
    pub proxied: bool,
}

/// 代理流条目（`GET /api/proxies` 的数组元素）。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProxyItem {
    pub stream_id: StreamId,
    pub upstream: String,
}

/// WebRTC 信令开始请求（`POST /api/webrtc/start`）。
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebRtcStartReq {
    pub stream_id: StreamId,
}

/// WebRTC 信令开始响应（`POST /api/webrtc/start`）。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebRtcStartResp {
    pub peer_id: String,
    pub sdp: String,
}

/// WebRTC 信令提交响应（`POST /api/webrtc/answer`）。
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebRtcAnswerReq {
    pub peer_id: String,
    pub sdp: String,
}

/// WebRTC 信令提交响应（`POST /api/webrtc/answer`）。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebRtcAnswerResp {
    pub ok: bool,
}
