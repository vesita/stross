//! HTTP 层：路由 / 静态页面 / REST API / WebSocket 升级 / WebRTC 信令。
//!
//! 数据面转发逻辑在 [`super::handle_push`] / [`super::handle_watch`]（传输无关）；
//! 本模块只负责把 HTTP/WS 入口接到转发逻辑上。

use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use stross_proto::message::StreamInfo;

use crate::relay::{PeerInfo, RelayState, handle_push, handle_watch};
use crate::transport::webrtc::{PeerCommand, WebRtcTransport};
use crate::transport::ws::WsTransport;

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

/// 组装中继的 HTTP 路由（静态页面 + REST API + WebSocket 升级）。
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
        .layer(axum::middleware::from_fn(cors_layer))
        .with_state(state)
}

/// 中继入口信息（各传输端口；前端据此构造 srt:// / quic:// 拨号地址）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RelayInfoResp {
    /// HTTP/WS 端口。
    port: u16,
    /// SRT 推流/观看端口（随机分配）。
    srt_port: Option<u16>,
    /// QUIC 推流/观看端口（随机分配）。
    quic_port: Option<u16>,
}

async fn api_info(State(state): State<RelayState>) -> Json<RelayInfoResp> {
    Json(RelayInfoResp {
        port: state.port,
        srt_port: state.srt_port,
        quic_port: state.quic_port,
    })
}

async fn api_streams(State(state): State<RelayState>) -> Json<Vec<StreamInfo>> {
    Json(state.streams())
}

/// 局域网内其它设备（观看端页面「局域网设备」区拉取）。
async fn api_peers(State(state): State<RelayState>) -> Json<Vec<PeerInfo>> {
    Json(state.peers())
}

// ---------------------------------------------------------------------------
// 级联代理（转发链/树）：POST /api/proxy 把上游中继的流拉到本地广播
// ---------------------------------------------------------------------------

/// 请求建立代理流。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProxyReq {
    /// 上游中继基址（`ws://host:port`；`srt://` / `quic://` 亦可）。
    upstream: String,
    /// 上游流 id。
    stream_id: String,
    /// 上游流信息（可选；前端自动发现时已持有，透传避免再向上游查询）。
    #[serde(default)]
    info: Option<StreamInfo>,
}

/// 建立代理流：本地 `/api/streams` 立即出现该流，普通 watch 即可订阅。
/// 409 = 本地已有同名流（推流或代理）。
async fn api_proxy_start(
    State(state): State<RelayState>,
    Json(req): Json<ProxyReq>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    match state.start_proxy(&req.upstream, &req.stream_id, req.info) {
        Ok(id) => Ok(Json(serde_json::json!({
            "streamId": id,
            "proxied": true,
        }))),
        Err(e) => Err(api_err(StatusCode::CONFLICT, e)),
    }
}

/// 列出本中继当前代理的流（id → 上游）。
async fn api_proxies(State(state): State<RelayState>) -> Json<Vec<serde_json::Value>> {
    Json(
        state
            .proxies()
            .into_iter()
            .map(|(stream_id, upstream)| {
                serde_json::json!({ "streamId": stream_id, "upstream": upstream })
            })
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// WebRTC 观看端信令（数据面与 WS 观看端共用 handle_watch）
// ---------------------------------------------------------------------------

static NEXT_WEBRTC_PEER: AtomicU64 = AtomicU64::new(1);

type ApiErr = (StatusCode, Json<serde_json::Value>);

fn api_err(status: StatusCode, msg: impl Into<String>) -> ApiErr {
    (status, Json(serde_json::json!({ "error": msg.into() })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebRtcStartReq {
    stream_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WebRtcStartResp {
    peer_id: String,
    sdp: String,
}

/// 开始 WebRTC 观看信令：创建 peer（control + media 双通道），返回 SDP offer。
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebRtcAnswerReq {
    peer_id: String,
    sdp: String,
}

/// 提交观看端 answer：接入 peer，双通道打开后启动与 WS 完全相同的转发逻辑。
async fn api_webrtc_answer(
    State(state): State<RelayState>,
    Json(req): Json<WebRtcAnswerReq>,
) -> Result<Json<serde_json::Value>, ApiErr> {
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
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
struct WatchQuery {
    stream: String,
}

async fn ws_watch(
    ws: WebSocketUpgrade,
    Query(q): Query<WatchQuery>,
    State(state): State<RelayState>,
) -> Response {
    ws.on_upgrade(move |socket| {
        let session = WsTransport::new().from_upgraded(socket);
        handle_watch(session, q.stream, state)
    })
}

async fn ws_push(ws: WebSocketUpgrade, State(state): State<RelayState>) -> Response {
    ws.on_upgrade(move |socket| {
        let session = WsTransport::new().from_upgraded(socket);
        handle_push(session, state)
    })
}
