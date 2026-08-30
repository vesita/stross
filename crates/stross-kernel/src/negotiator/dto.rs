//! 凭证协商 REST API 的数据传输对象（DTO）。
//!
//! 请求/响应体直接复用 stross-proto 的线协议类型（已带 `ToSchema`）：
//! [`ShareRequest`]（`POST /api/negotiator/request` 请求体）、
//! [`ShareGrant`]（签发结果）、[`EndpointDir`]（`GET /api/endpoints` 响应）。
//! 错误体统一 [`ApiError`]。

use utoipa::ToSchema;

pub use stross_proto::message::{RelayAddr, ShareGrant, ShareRequest, ShareTokenView};

/// 错误响应体（统一 `{ "error": "..." }`）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiError {
    pub error: String,
}
