//! 凭证协商端点的 HTTP 路由 / OpenAPI 声明。
//!
//! 处理器（[`super::handle_request`] / [`super::handle_endpoints`]）与协商逻辑同住
//! [`super`]（`mod.rs`），此处只负责路由组装 + OpenAPI 收集 + CORS；DTO 见
//! [`super::dto`]，wire 类型见 `stross_proto::message::negotiator`。

use axum::Router;
use axum::routing::{get, post};
use std::sync::Arc;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use super::{ServerState, handle_discovery, handle_endpoints, handle_request};

/// OpenAPI 文档（`/api-docs/openapi.json` + swagger-ui /docs）。
#[derive(OpenApi)]
#[openapi(
    paths(
        crate::negotiator::handle_request,
        crate::negotiator::handle_endpoints,
        crate::negotiator::handle_discovery
    ),
    tags((name = "negotiator", description = "凭证自动协商：申请出站凭证 / 目录 / 统一发现"))
)]
pub(crate) struct ApiDoc;

/// CORS 中间件：Tauri 前端运行在本地源（`tauri://localhost`），跨源访问
/// 协商端点（POST + `Content-Type: application/json` 会触发预检），必须允许
/// 任意来源（与中继 HTTP 层的 cors_layer 语义一致——LAN 可信模型下不限定来源）。
pub(super) async fn cors_layer(
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = req.method().clone();
    let mut resp = next.run(req).await;
    let headers = resp.headers_mut();
    headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        axum::http::HeaderValue::from_static("*"),
    );
    headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_METHODS,
        axum::http::HeaderValue::from_static("POST, GET, OPTIONS"),
    );
    headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_HEADERS,
        axum::http::HeaderValue::from_static("Content-Type"),
    );
    // 预检直接放行（axum 对 OPTIONS 无路由 → 404；这里显式返回 204）
    if method == axum::http::Method::OPTIONS {
        resp = axum::response::Response::builder()
            .status(axum::http::StatusCode::NO_CONTENT)
            .body(axum::body::Body::empty())
            .expect("静态响应");
        resp.headers_mut().insert(
            axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
            axum::http::HeaderValue::from_static("*"),
        );
        resp.headers_mut().insert(
            axum::http::header::ACCESS_CONTROL_ALLOW_METHODS,
            axum::http::HeaderValue::from_static("POST, GET, OPTIONS"),
        );
        resp.headers_mut().insert(
            axum::http::header::ACCESS_CONTROL_ALLOW_HEADERS,
            axum::http::HeaderValue::from_static("Content-Type"),
        );
        return resp;
    }
    resp
}

/// 组装协商端点路由（`POST /api/negotiator/request` + `GET /api/endpoints` + /docs）。
pub(super) fn router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/api/negotiator/request", post(handle_request))
        .route("/api/endpoints", get(handle_endpoints))
        .route("/api/discovery", get(handle_discovery))
        .merge(SwaggerUi::new("/docs").url("/api-docs/openapi.json", ApiDoc::openapi()))
        .layer(axum::middleware::from_fn(cors_layer))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openapi_generates_with_negotiator_paths() {
        let json = serde_json::to_value(ApiDoc::openapi()).expect("OpenAPI 应可序列化");
        let paths = json["paths"].as_object().expect("应有 paths");
        assert!(
            paths.contains_key("/api/negotiator/request"),
            "缺少 /api/negotiator/request"
        );
        assert!(paths.contains_key("/api/endpoints"), "缺少 /api/endpoints");
        assert!(paths.contains_key("/api/discovery"), "缺少 /api/discovery");
        let schemas = json["components"]["schemas"]
            .as_object()
            .expect("应有 schemas");
        for s in [
            "ShareRequest",
            "ShareGrant",
            "ApiError",
            "EndpointDir",
            "DiscoveryResp",
        ] {
            assert!(schemas.contains_key(s), "缺少 schema {s}");
        }
    }
}
