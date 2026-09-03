//! HTTP 层：路由 / REST API / WebSocket 升级 / WebRTC 信令。
//!
//! 数据面转发逻辑在 [`super::data_plane::handle_push`] / [`super::data_plane::handle_watch`]
//! （传输无关）；本模块负责把 HTTP/WS 入口接到转发逻辑上。
//!
//! 结构（相对旧的单文件 http.rs）：DTO 收敛到 [`super::dto`]，REST 处理器在此
//! 用 `#[utoipa::path]` 声明 OpenAPI（[`ApiDoc`]），并挂 swagger-ui 于 `/docs`。

use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{ConnectInfo, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use tokio::sync::Mutex;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use stross_proto::message::StreamInfo;

use crate::relay::data_plane::{handle_push, handle_watch};
use crate::relay::dto::{
    ApiError, ProxyItem, ProxyReq, ProxyStartResp, RelayInfoResp, WebRtcAnswerReq,
    WebRtcAnswerResp, WebRtcStartReq, WebRtcStartResp,
};
use crate::relay::{PeerInfo, RelayState};
use crate::transport::TransportError;
use crate::transport::webrtc::{PeerCommand, WebRtcTransport};
use crate::transport::ws::WsTransport;

/// OpenAPI 文档（`/api-docs/openapi.json` + swagger-ui /docs）。
#[derive(OpenApi)]
#[openapi(
    paths(
        api_info,
        api_streams,
        api_peers,
        api_proxy_start,
        api_proxies,
        api_webrtc_start,
        api_webrtc_answer
    ),
    tags((name = "relay", description = "中继 REST API：入端口 / 流 / 设备 / 级联代理 / WebRTC 信令"))
)]
pub(crate) struct ApiDoc;

/// CORS 中间件：Stross 桌面/Android 前端运行在 Tauri 的本地源
/// （`tauri://localhost` / `http://tauri.localhost`），连接阶段会跨源
/// 访问中继的 `/api/*`，必须允许任意来源。
async fn cors_layer(
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let mut resp = next.run(req).await;
    resp.headers_mut().insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        axum::http::HeaderValue::from_static("*"),
    );
    resp
}

/// 组装中继的 HTTP 路由（REST API + WebSocket 升级 + /docs swagger-ui）。
pub(super) fn router(state: RelayState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/info", get(api_info))
        .route("/api/streams", get(api_streams))
        .route("/api/peers", get(api_peers))
        .route("/api/proxy", post(api_proxy_start))
        .route("/api/proxies", get(api_proxies))
        .route("/api/webrtc/start", post(api_webrtc_start))
        .route("/api/webrtc/answer", post(api_webrtc_answer))
        .route("/ws/push", get(ws_push))
        .route("/ws/watch", get(ws_watch))
        .route("/ws/channel", get(ws_channel))
        .merge(SwaggerUi::new("/docs").url("/api-docs/openapi.json", ApiDoc::openapi()))
        .layer(axum::middleware::from_fn(cors_layer))
        .with_state(state)
}

/// 中继入口信息。前端据此构造 `srt://` / `quic://` 拨号地址。
#[utoipa::path(
    get,
    path = "/api/info",
    tag = "relay",
    responses((status = 200, description = "中继入口信息", body = RelayInfoResp))
)]
async fn api_info(State(state): State<RelayState>) -> Json<RelayInfoResp> {
    Json(RelayInfoResp {
        port: state.port,
        srt_port: state.srt_port,
        quic_port: state.quic_port,
    })
}

/// 当前在线流列表（本地推流 + 代理流）。
#[utoipa::path(
    get,
    path = "/api/streams",
    tag = "relay",
    responses((status = 200, description = "在线流列表", body = [StreamInfo]))
)]
async fn api_streams(State(state): State<RelayState>) -> Json<Vec<StreamInfo>> {
    Json(state.streams())
}

/// 局域网内其它设备（观看端页面「局域网设备」区拉取）。
#[utoipa::path(
    get,
    path = "/api/peers",
    tag = "relay",
    responses((status = 200, description = "局域网设备", body = [PeerInfo]))
)]
async fn api_peers(State(state): State<RelayState>) -> Json<Vec<PeerInfo>> {
    Json(state.peers())
}

/// 建立代理流：本地 `/api/streams` 立即出现该流，普通 watch 即可订阅。
/// 409 = 本地已有同名流（推流或代理）。
#[utoipa::path(
    post,
    path = "/api/proxy",
    tag = "relay",
    request_body = ProxyReq,
    responses(
        (status = 200, description = "代理已建立", body = ProxyStartResp),
        (status = 409, description = "本地已有同名流", body = ApiError)
    )
)]
async fn api_proxy_start(
    State(state): State<RelayState>,
    Json(req): Json<ProxyReq>,
) -> Result<Json<ProxyStartResp>, ApiErr> {
    match state.start_proxy(&req.upstream, &req.stream_id, req.info) {
        Ok(id) => Ok(Json(ProxyStartResp {
            stream_id: id,
            proxied: true,
        })),
        Err(e) => Err(api_err(StatusCode::CONFLICT, e.to_string())),
    }
}

/// 列出本中继当前代理的流（id → 上游）。
#[utoipa::path(
    get,
    path = "/api/proxies",
    tag = "relay",
    responses((status = 200, description = "代理流列表", body = [ProxyItem]))
)]
async fn api_proxies(State(state): State<RelayState>) -> Json<Vec<ProxyItem>> {
    Json(
        state
            .proxies()
            .into_iter()
            .map(|(stream_id, upstream)| ProxyItem {
                stream_id,
                upstream,
            })
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// WebRTC 观看端信令（数据面与 WS 观看端共用 handle_watch）
// ---------------------------------------------------------------------------

static NEXT_WEBRTC_PEER: AtomicU64 = AtomicU64::new(1);

type ApiErr = (StatusCode, Json<ApiError>);

fn api_err(status: StatusCode, msg: impl Into<String>) -> ApiErr {
    (status, Json(ApiError { error: msg.into() }))
}

/// 开始 WebRTC 观看信令：创建 peer（control + media 双通道），返回 SDP offer。
#[utoipa::path(
    post,
    path = "/api/webrtc/start",
    tag = "relay",
    request_body = WebRtcStartReq,
    responses(
        (status = 200, description = "信令已开始（SDP offer）", body = WebRtcStartResp),
        (status = 500, description = "创建 peer 失败", body = ApiError)
    )
)]
async fn api_webrtc_start(
    State(state): State<RelayState>,
    Json(req): Json<WebRtcStartReq>,
) -> Result<Json<WebRtcStartResp>, ApiErr> {
    let transport = WebRtcTransport::new();
    let bind = "0.0.0.0:0".parse().expect("静态地址");
    let (sdp, peer) = transport
        .start_peer(&req.stream_id, bind)
        .await
        .map_err(|e| api_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let peer_id = format!("w{:x}", NEXT_WEBRTC_PEER.fetch_add(1, Ordering::Relaxed));
    state
        .webrtc_peers
        .lock()
        .unwrap()
        .insert(peer_id.clone(), peer);
    // 看门狗：start 后 30s 未提交 answer 的 peer 回收（防止 UDP socket / 状态泄漏）
    let state_cleanup = state.clone();
    let peer_id_cleanup = peer_id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        if state_cleanup
            .webrtc_peers
            .lock()
            .unwrap()
            .remove(&peer_id_cleanup)
            .is_some()
        {
            tracing::warn!("webrtc peer {peer_id_cleanup} 30s 未完成信令，已回收");
        }
    });
    tracing::info!("webrtc 信令开始: peer={peer_id} stream={}", req.stream_id);
    Ok(Json(WebRtcStartResp { peer_id, sdp }))
}

/// 提交观看端 answer：接入 peer，双通道打开后启动与 WS 完全相同的转发逻辑。
#[utoipa::path(
    post,
    path = "/api/webrtc/answer",
    tag = "relay",
    request_body = WebRtcAnswerReq,
    responses(
        (status = 200, description = "已接入 peer", body = WebRtcAnswerResp),
        (status = 404, description = "peer 不存在或已使用", body = ApiError),
        (status = 400, description = "answer 校验失败", body = ApiError)
    )
)]
async fn api_webrtc_answer(
    State(state): State<RelayState>,
    Json(req): Json<WebRtcAnswerReq>,
) -> Result<Json<WebRtcAnswerResp>, ApiErr> {
    let mut peer = state
        .webrtc_peers
        .lock()
        .unwrap()
        .remove(&req.peer_id)
        .ok_or_else(|| {
            api_err(
                StatusCode::NOT_FOUND,
                format!("peer 不存在或已使用: {}", req.peer_id),
            )
        })?;
    let stream_id = peer.session_id().to_string();
    let (session, mut channels_open, close_tx) = peer
        .accept_answer(&req.sdp)
        .await
        .map_err(|e| api_err(StatusCode::BAD_REQUEST, e.to_string()))?;

    tokio::spawn(async move {
        // 等待 control/media 双通道打开（最多 15s），然后复用 WS 观看端转发逻辑
        if !*channels_open.borrow()
            && tokio::time::timeout(std::time::Duration::from_secs(15), channels_open.changed())
                .await
                .is_err()
        {
            tracing::warn!("webrtc 通道 15s 未打开，关闭 peer（stream={stream_id}）");
            let _ = close_tx.send(PeerCommand::Close).await;
            return;
        }
        handle_watch(session, stream_id, state).await;
    });
    Ok(Json(WebRtcAnswerResp { ok: true }))
}

#[derive(Deserialize)]
struct WatchQuery {
    stream: String,
}

async fn ws_watch(
    ws: WebSocketUpgrade,
    Query(q): Query<WatchQuery>,
    State(state): State<RelayState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
) -> Response {
    ws.on_upgrade(move |socket| {
        let session = WsTransport::new().from_socket(Box::new(AxumWs::new(socket)), Some(peer));
        handle_watch(session, q.stream, state)
    })
}

async fn ws_push(
    ws: WebSocketUpgrade,
    State(state): State<RelayState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
) -> Response {
    ws.on_upgrade(move |socket| {
        let session = WsTransport::new().from_socket(Box::new(AxumWs::new(socket)), Some(peer));
        handle_push(session, state)
    })
}

#[derive(Debug, Deserialize)]
struct ChannelQuery {
    peer_id: String,
    #[serde(default)]
    peer_name: Option<String>,
}

async fn ws_channel(
    ws: WebSocketUpgrade,
    Query(q): Query<ChannelQuery>,
    State(state): State<RelayState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
) -> Response {
    let Some(mgr) = state.channel_manager() else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "ChannelManager 未就绪",
        )
            .into_response();
    };
    let peer_id = crate::kernel::id::Id::from(q.peer_id);
    let peer_name = q.peer_name.unwrap_or_else(|| "Unknown".into());
    ws.on_upgrade(move |socket| async move {
        let session = WsTransport::new().from_socket(Box::new(AxumWs::new(socket)), Some(peer));
        let chan = mgr.register_session(peer_id, &peer_name, session).await;
        chan.wait_closed().await;
    })
}

/// axum 服务端 WS socket 适配：读写分离架构（SplitSink + SplitStream），
/// 避免全双工通道收发互锁。
type AxumWsSink =
    futures_util::stream::SplitSink<axum::extract::ws::WebSocket, axum::extract::ws::Message>;
type AxumWsStream = futures_util::stream::SplitStream<axum::extract::ws::WebSocket>;

struct AxumWs {
    sink: Mutex<Option<AxumWsSink>>,
    stream: Mutex<Option<AxumWsStream>>,
}

impl AxumWs {
    fn new(socket: axum::extract::ws::WebSocket) -> Self {
        use futures_util::StreamExt;
        let (sink, stream) = socket.split();
        Self {
            sink: Mutex::new(Some(sink)),
            stream: Mutex::new(Some(stream)),
        }
    }
}

#[async_trait]
impl stross_transport::ws::WsIo for AxumWs {
    async fn send_msg(&self, msg: stross_transport::ws::WsMsg) -> Result<(), TransportError> {
        use axum::extract::ws::Message as M;
        use futures_util::SinkExt;
        let msg = match msg {
            stross_transport::ws::WsMsg::Text(s) => M::Text(s.into()),
            stross_transport::ws::WsMsg::Binary(b) => M::Binary(b),
        };
        let mut guard = self.sink.lock().await;
        let sink = guard.as_mut().ok_or(TransportError::Closed)?;
        sink.send(msg)
            .await
            .map_err(|e| TransportError::Io(e.to_string()))
    }
    async fn recv_msg(&self) -> Result<Option<stross_transport::ws::WsMsg>, TransportError> {
        use axum::extract::ws::Message as M;
        use futures_util::StreamExt;
        loop {
            let item = {
                let mut guard = self.stream.lock().await;
                let stream = guard.as_mut().ok_or(TransportError::Closed)?;
                stream.next().await
            };
            match item {
                Some(Ok(M::Text(t))) => {
                    return Ok(Some(stross_transport::ws::WsMsg::Text(t.to_string())));
                }
                Some(Ok(M::Binary(b))) => return Ok(Some(stross_transport::ws::WsMsg::Binary(b))),
                Some(Ok(M::Close(_))) | None => return Ok(None),
                Some(Ok(M::Ping(p))) => {
                    use futures_util::SinkExt;
                    let mut guard = self.sink.lock().await;
                    if let Some(sink) = guard.as_mut() {
                        let _ = sink.send(M::Pong(p)).await;
                    }
                    continue;
                }
                Some(Ok(M::Pong(_))) => continue,
                Some(Err(e)) => return Err(TransportError::Io(e.to_string())),
            }
        }
    }

    async fn close(&self) -> Result<(), TransportError> {
        use futures_util::SinkExt;
        let mut sink_guard = self.sink.lock().await;
        if let Some(mut sink) = sink_guard.take() {
            let _ = sink.send(axum::extract::ws::Message::Close(None)).await;
            let _ = sink.close().await;
        }
        let mut stream_guard = self.stream.lock().await;
        stream_guard.take();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// OpenAPI 文档能生成且路径齐全（dto/api/utoipa 声明闭环）。
    #[test]
    fn openapi_generates_with_all_relay_paths() {
        let spec = ApiDoc::openapi();
        let json = serde_json::to_value(&spec).expect("OpenAPI 应可序列化");
        let paths = json["paths"].as_object().expect("应有 paths");
        for p in [
            "/api/info",
            "/api/streams",
            "/api/peers",
            "/api/proxy",
            "/api/proxies",
            "/api/webrtc/start",
            "/api/webrtc/answer",
        ] {
            assert!(paths.contains_key(p), "缺少路径 {p}");
        }
        // 关键 schema 名称齐全
        let schemas = json["components"]["schemas"]
            .as_object()
            .expect("应有 schemas");
        for s in [
            "RelayInfoResp",
            "ProxyReq",
            "ProxyStartResp",
            "ProxyItem",
            "WebRtcStartReq",
            "WebRtcStartResp",
            "WebRtcAnswerReq",
            "WebRtcAnswerResp",
            "ApiError",
            "StreamInfo",
            "PeerInfo",
        ] {
            assert!(schemas.contains_key(s), "缺少 schema {s}");
        }
    }
}
