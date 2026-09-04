//! 凭证协商 REST API 的数据传输对象（DTO）。
//!
//! 请求/响应体直接复用 stross-proto 的线协议类型（已带 `ToSchema`）：
//! [`ShareRequest`]（`POST /api/negotiator/request` 请求体）、
//! [`ShareGrant`]（签发结果）、[`EndpointDir`]（`GET /api/endpoints` 响应）。
//! 错误体统一 [`ApiError`]。

use stross_proto::message::MediaKind;
use stross_view::id::NodeId;
use utoipa::ToSchema;

pub use stross_proto::message::{RelayAddr, ShareGrant, ShareRequest, ShareTokenView};

/// 错误响应体（统一 `{ "error": "..." }`）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiError {
    pub error: String,
}

/// 取消订阅请求体（`POST /api/negotiator/unsubscribe`）：订阅方在终止接收时
/// 显式通知共享方（共享端点据此即时更新「订阅中」状态，不必等数据面
/// watchers 断连检测）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UnsubscribeRequest {
    /// 取消订阅的节点 id（订阅方向共享方出示；共享方按 (端点, 节点) 移除）。
    pub node_id: NodeId,
    /// 订阅目标端点 kind（与 `endpoint_id` 组合成内部 [`EndpointId`]）。
    pub endpoint_kind: MediaKind,
    /// 订阅目标端点数值子 id。
    pub endpoint_id: u32,
}

/// 取消订阅响应（共享方确认；`remainingSubscribers` = 移除后该端点剩余订阅者数）。
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UnsubscribeResp {
    pub remaining_subscribers: u32,
}
