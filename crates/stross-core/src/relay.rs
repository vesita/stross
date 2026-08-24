//! 中继服务器：接收推流，向观看者广播。
//!
//! 借鉴 [MediaMTX](https://github.com/bluenviron/mediamtx) 的"推流端 → 中继 → 观看端"模型：
//!
//! * `GET /ws/push`：推流端 WebSocket（先发 `Hello`，再发二进制媒体帧）
//! * `GET /ws/watch?stream=<id>`：观看端 WebSocket（收到 `Ready` 后收帧）
//! * `GET /api/streams`：流列表（观看端页面拉取）
//! * `GET /`：内嵌的观看端页面
//!
//! 数据面转发（[`handle_push`] / [`handle_watch`]）只依赖
//! [`Transport`](crate::transport::Transport) 抽象，不感知具体传输；
//! 当前经 [`WsTransport`](crate::transport::ws::WsTransport) 从 HTTP 升级处接入
//! （见 docs/plugin-architecture.md §4）。
//!
//! 观看端接入时机：视频只在关键帧（IDR）后开始转发（ffmpeg 已在关键帧前重复
//! SPS/PPS，因此等待关键帧即可）；音频 ADTS 自带配置，可直接转发。

use std::collections::HashMap;
#[cfg(feature = "discovery")]
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Query, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use stross_proto::frame::{Frame, TRACK_VIDEO};
use stross_proto::message::{ControlMessage, StreamInfo};

use crate::assets;
use crate::transport::quic::QuicTransport;
use crate::transport::srt::SrtTransport;
use crate::transport::webrtc::{PeerCommand, WebRtcPeer, WebRtcTransport};
use crate::transport::ws::WsTransport;
use crate::transport::{DataSession, SessionPacket};

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

/// 默认中继端口。
pub const DEFAULT_PORT: u16 = 8777;

/// 单条流的内部状态。
#[derive(Clone)]
struct StreamEntry {
    info: StreamInfo,
    tx: broadcast::Sender<Frame>,
    /// 最近一个视频关键帧（含 SPS/PPS），供新观看者立即对齐 GOP。
    last_keyframe: Option<Frame>,
}

/// 一个局域网内可发现的 Stross 中继（设备）。
///
/// 由 mDNS 浏览结果构造（feature `discovery` 的周期任务维护），
/// 也可手动注册（[`RelayState::insert_peer`]，调试 / 测试 / 跨网段补充）。
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerInfo {
    /// 唯一标识（`ip:port`）。
    pub id: String,
    /// 设备名（TXT `name`；缺失时回退为 `ip:port`）。
    pub name: String,
    pub ip: String,
    pub port: u16,
    /// 角色（TXT `roles`，逗号分隔：sender / viewer / relay）。
    pub roles: Vec<String>,
    /// 支持的传输（TXT `transports`：ws / webrtc / srt / quic）。
    pub transports: Vec<String>,
    /// 观看页地址（`http://ip:port/`）。
    pub url: String,
}

/// 中继共享状态。
#[derive(Clone, Default)]
pub struct RelayState {
    streams: Arc<Mutex<HashMap<String, StreamEntry>>>,
    /// 待完成信令的 WebRTC peer（`/api/webrtc/start` 与 `/answer` 之间）。
    webrtc_peers: Arc<Mutex<HashMap<String, WebRtcPeer>>>,
    /// 局域网内其它中继（设备发现缓存；`/api/peers` 返回）。
    peers: Arc<Mutex<HashMap<String, PeerInfo>>>,
}

impl RelayState {
    /// 流列表（快照）。
    pub fn streams(&self) -> Vec<StreamInfo> {
        let guard = self.streams.lock().unwrap();
        let mut v: Vec<_> = guard
            .values()
            .map(|e| {
                let mut info = e.info.clone();
                info.watchers = e.tx.receiver_count() as u32;
                info
            })
            .collect();
        v.sort_by(|a, b| a.stream_id.cmp(&b.stream_id));
        v
    }

    fn get(&self, id: &str) -> Option<StreamEntry> {
        self.streams.lock().unwrap().get(id).cloned()
    }

    fn insert(&self, entry: StreamEntry) {
        self.streams
            .lock()
            .unwrap()
            .insert(entry.info.stream_id.clone(), entry);
    }

    fn set_last_keyframe(&self, id: &str, frame: Frame) {
        let mut guard = self.streams.lock().unwrap();
        if let Some(entry) = guard.get_mut(id) {
            entry.last_keyframe = Some(frame);
        }
    }

    fn remove(&self, id: &str) {
        self.streams.lock().unwrap().remove(id);
    }

    /// 局域网设备列表（按名称排序）。
    pub fn peers(&self) -> Vec<PeerInfo> {
        let mut v: Vec<_> = self.peers.lock().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name).then(a.port.cmp(&b.port)));
        v
    }

    /// 整体替换局域网设备表（mDNS 周期浏览结果）。
    pub fn set_peers(&self, peers: HashMap<String, PeerInfo>) {
        *self.peers.lock().unwrap() = peers;
    }

    /// 手动注册一台中继（调试 / 测试 / 手动补充跨网段设备）。
    pub fn insert_peer(&self, peer: PeerInfo) {
        self.peers.lock().unwrap().insert(peer.id.clone(), peer);
    }
}

/// 中继句柄。
pub struct RelayHandle {
    /// 实际监听端口（绑定 0 时由系统分配）。
    pub port: u16,
    /// SRT 推流端口（随中继启动，独立 UDP；`None` = 未启用）。
    pub srt_port: Option<u16>,
    /// QUIC 推流端口（随中继启动，独立 UDP；`None` = 未启用）。
    pub quic_port: Option<u16>,
    state: RelayState,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl RelayHandle {
    /// 当前流列表。
    pub fn streams(&self) -> Vec<StreamInfo> {
        self.state.streams()
    }

    /// 局域网内其它设备（mDNS 发现缓存）。
    pub fn peers(&self) -> Vec<PeerInfo> {
        self.state.peers()
    }

    /// 手动注册一台局域网设备（调试 / 测试用）。
    pub fn insert_peer(&self, peer: PeerInfo) {
        self.state.insert_peer(peer);
    }

    /// 停止中继服务。
    pub async fn stop(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }
}

impl RelayServer {
    /// 绑定并启动中继。
    ///
    /// `port == 0` 时由系统分配空闲端口（测试用），实际端口在
    /// 返回的 [`RelayHandle::port`] 上；SRT/QUIC 推流监听随机端口，
    /// 见 [`RelayHandle::srt_port`] / [`RelayHandle::quic_port`]。
    pub async fn start(port: u16) -> anyhow::Result<RelayHandle> {
        let state = RelayState::default();
        let app = Router::new()
            .route("/", get(serve_index))
            .route("/style.css", get(serve_style))
            .route("/app.js", get(serve_app_js))
            .route("/jmuxer.js", get(serve_jmuxer))
            .route("/healthz", get(|| async { "ok" }))
            .route("/api/streams", get(api_streams))
            .route("/api/peers", get(api_peers))
            .route("/api/webrtc/start", post(api_webrtc_start))
            .route("/api/webrtc/answer", post(api_webrtc_answer))
            .route("/ws/push", get(ws_push))
            .route("/ws/watch", get(ws_watch))
            .layer(axum::middleware::from_fn(cors_layer))
            .with_state(state.clone());

        let listener = TcpListener::bind(("0.0.0.0", port)).await?;
        let actual_port = listener.local_addr()?.port();
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

        // SRT 推流监听：原生推流端可经 srt://<host>:<srt_port> 推流，
        // 数据面与 WS 推流完全一致（handle_push；传输抽象第三次验证）
        let mut srt_listener = SrtTransport::new()
            .bind("0.0.0.0:0")
            .await
            .map_err(|e| anyhow::anyhow!("SRT 监听失败: {e}"))?;
        let srt_port = srt_listener.local_addr().port();
        tracing::info!("SRT 推流监听: 0.0.0.0:{srt_port}");
        let srt_state = state.clone();
        let mut srt_shutdown = shutdown_tx.subscribe();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    res = srt_listener.accept() => match res {
                        Ok(session) => {
                            let state = srt_state.clone();
                            tokio::spawn(async move { handle_push(session, state).await });
                        }
                        Err(e) => {
                            tracing::warn!("SRT accept 失败: {e}");
                            break;
                        }
                    },
                    _ = srt_shutdown.changed() => break,
                }
            }
            tracing::debug!("SRT 监听已停止");
        });

        // QUIC 推流监听：一条连接 control/media 双 stream 多路复用
        // （Lossless；自签名证书 + 客户端接受任意证书，局域网可信模型）
        let mut quic_listener = QuicTransport::new()
            .bind("0.0.0.0:0".parse().expect("静态地址"))
            .await
            .map_err(|e| anyhow::anyhow!("QUIC 监听失败: {e}"))?;
        let quic_port = quic_listener.local_addr().port();
        tracing::info!("QUIC 推流监听: 0.0.0.0:{quic_port}");
        let quic_state = state.clone();
        let mut quic_shutdown = shutdown_tx.subscribe();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    res = quic_listener.accept() => match res {
                        Ok(session) => {
                            let state = quic_state.clone();
                            tokio::spawn(async move { handle_push(session, state).await });
                        }
                        Err(e) => {
                            tracing::warn!("QUIC accept 失败: {e}");
                            break;
                        }
                    },
                    _ = quic_shutdown.changed() => break,
                }
            }
            tracing::debug!("QUIC 监听已停止");
        });

        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.changed().await;
                })
                .await;
        });
        // 周期浏览局域网内其它中继，维护设备发现缓存（feature `discovery`）
        #[cfg(feature = "discovery")]
        spawn_peer_refresh(state.clone(), actual_port, shutdown_tx.subscribe());
        tracing::info!("中继已启动: 0.0.0.0:{actual_port}");
        Ok(RelayHandle {
            port: actual_port,
            srt_port: Some(srt_port),
            quic_port: Some(quic_port),
            state,
            shutdown: shutdown_tx,
            task,
        })
    }
}

/// 中继服务器构造器（单方法命名空间）。
pub struct RelayServer;

// ---------------------------------------------------------------------------
// HTTP / WS 处理
// ---------------------------------------------------------------------------

async fn serve_index() -> Response {
    static_html(assets::INDEX_HTML, "text/html; charset=utf-8")
}

async fn serve_style() -> Response {
    static_html(assets::STYLE_CSS, "text/css; charset=utf-8")
}

async fn serve_app_js() -> Response {
    static_html(assets::APP_JS, "text/javascript; charset=utf-8")
}

async fn serve_jmuxer() -> Response {
    static_html(assets::JMUXER_JS, "text/javascript; charset=utf-8")
}

fn static_html(body: &'static str, mime: &'static str) -> Response {
    let mut resp = Response::new(body.into());
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(mime));
    resp.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    resp
}

async fn api_streams(State(state): State<RelayState>) -> Json<Vec<StreamInfo>> {
    Json(state.streams())
}

/// 局域网内其它设备（观看端页面「局域网设备」区拉取）。
async fn api_peers(State(state): State<RelayState>) -> Json<Vec<PeerInfo>> {
    Json(state.peers())
}

// ---------------------------------------------------------------------------
// 设备发现缓存（feature `discovery`）：周期 mDNS 浏览局域网内其它中继
// ---------------------------------------------------------------------------

/// 周期浏览局域网内其它 Stross 中继，更新 [`RelayState`] 的设备缓存。
///
/// 每 `BROWSE_INTERVAL` 浏览一次（每次 `Discovery::browse` 内置超时）；
/// 被浏览到的本机广播实例（本机 IP + 本机端口）会被剔除。
#[cfg(feature = "discovery")]
fn spawn_peer_refresh(state: RelayState, self_port: u16, mut shutdown: watch::Receiver<bool>) {
    const BROWSE_INTERVAL: Duration = Duration::from_secs(15);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(BROWSE_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    match crate::discovery::Discovery::browse(Duration::from_secs(2)).await {
                        Ok(found) => {
                            let self_ips = crate::net::local_ips();
                            let peers = filter_self(found, self_port, &self_ips);
                            state.set_peers(peers);
                        }
                        Err(e) => tracing::warn!("局域网设备浏览失败: {e}"),
                    }
                }
                _ = shutdown.changed() => break,
            }
        }
        tracing::debug!("设备发现缓存已停止");
    });
}

/// 从 mDNS 浏览结果剔除本机中继（自己广播的实例），并映射为设备表。
#[cfg(feature = "discovery")]
fn filter_self(
    found: Vec<crate::discovery::Discovered>,
    self_port: u16,
    self_ips: &[IpAddr],
) -> HashMap<String, PeerInfo> {
    let mut out = HashMap::new();
    for d in found {
        // 本机广播的实例：本机 IP + 本机中继端口
        if d.port == self_port && self_ips.contains(&d.ip) {
            continue;
        }
        out.insert(format!("{}:{}", d.ip, d.port), peer_from_discovered(d));
    }
    out
}

/// 把一条 mDNS 浏览记录映射为 [`PeerInfo`]（TXT 缺失时回退默认值）。
#[cfg(feature = "discovery")]
fn peer_from_discovered(d: crate::discovery::Discovered) -> PeerInfo {
    let txt = |k: &str| {
        d.txt
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.clone())
    };
    let split = |k: &str| -> Vec<String> {
        txt(k)
            .map(|v| {
                v.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    };
    let name = txt("name")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("{}:{}", d.ip, d.port));
    PeerInfo {
        id: format!("{}:{}", d.ip, d.port),
        name,
        ip: d.ip.to_string(),
        port: d.port,
        roles: split("roles"),
        transports: split("transports"),
        url: format!("http://{}:{}/", d.ip, d.port),
    }
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
        if !*channels_open.borrow() {
            if tokio::time::timeout(Duration::from_secs(15), channels_open.changed())
                .await
                .is_err()
            {
                tracing::warn!("webrtc 通道 15s 未打开，关闭 peer（stream={stream_id}）");
                let _ = close_tx.send(PeerCommand::Close).await;
                return;
            }
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

/// 观看端：等待 Ready，然后按关键帧对齐转发。
async fn handle_watch(session: Box<dyn DataSession>, stream_id: String, state: RelayState) {
    let Some(entry) = state.get(&stream_id) else {
        let _ = session
            .send(SessionPacket::Control(ControlMessage::Error {
                message: format!("流 {stream_id} 不存在"),
            }))
            .await;
        return;
    };
    let info = entry.info.clone();
    let mut rx = entry.tx.subscribe();
    let _ = session
        .send(SessionPacket::Control(ControlMessage::Ready {
            stream_id: stream_id.clone(),
        }))
        .await;

    let mut video_started = false;
    // 新观看者先收到最近一个关键帧（含 SPS/PPS），立刻可解码
    if let Some(kf) = entry.last_keyframe.clone() {
        if session.send(SessionPacket::Media(kf)).await.is_err() {
            return;
        }
        video_started = true;
    }

    loop {
        let frame = match rx.recv().await {
            Ok(f) => f,
            Err(broadcast::error::RecvError::Lagged(_)) => {
                // 掉帧：重新等下一个关键帧，避免从 GOP 中间开始
                video_started = false;
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        };
        if frame.header.track == TRACK_VIDEO
            && !video_started
            && !frame.header.is_keyframe()
            && !frame.header.is_config()
        {
            continue;
        }
        if frame.header.track == TRACK_VIDEO
            && (frame.header.is_keyframe() || frame.header.is_config())
        {
            video_started = true;
        }
        if session.send(SessionPacket::Media(frame)).await.is_err() {
            break;
        }
    }
    // 干净关闭会话（WS 发 Close 帧；WebRTC 触发 run loop 退出）
    let _ = session.close().await;
    tracing::debug!("观看端断开: {stream_id} ({})", info.title);
}

/// 推流端：Hello → 建流 → 转发帧；Bye / 断开 → 删流。
async fn handle_push(session: Box<dyn DataSession>, state: RelayState) {
    let mut stream_id: Option<String> = None;
    loop {
        let pkt = match session.recv().await {
            Ok(Some(pkt)) => pkt,
            Ok(None) => {
                tracing::warn!("推流端连接已断开（无更多消息）");
                break;
            }
            Err(e) => {
                tracing::warn!("推流端连接异常: {e}");
                break;
            }
        };
        match pkt {
            SessionPacket::Control(ControlMessage::Hello {
                stream_id: id,
                title,
                video,
                audio,
            }) => {
                // 同名流冲突时拒绝新推流端
                if state.get(&id).is_some() {
                    let _ = session
                        .send(SessionPacket::Control(ControlMessage::Error {
                            message: format!("流 {id} 已存在"),
                        }))
                        .await;
                    let _ = session.close().await;
                    return;
                }
                let (tx, _rx) = broadcast::channel(1024);
                let info = StreamInfo {
                    stream_id: id.clone(),
                    title,
                    video,
                    audio,
                    started_at: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                    watchers: 0,
                };
                state.insert(StreamEntry {
                    info: info.clone(),
                    tx,
                    last_keyframe: None,
                });
                stream_id = Some(id.clone());
                tracing::info!("推流开始: {} ({})", id, info.title);
                let _ = session
                    .send(SessionPacket::Control(ControlMessage::Welcome {
                        stream_id: id,
                    }))
                    .await;
            }
            SessionPacket::Control(ControlMessage::Bye) => {
                break;
            }
            SessionPacket::Control(_) => {}
            SessionPacket::Media(frame) => {
                if let Some(id) = &stream_id {
                    if frame.header.track == TRACK_VIDEO && frame.header.is_keyframe() {
                        state.set_last_keyframe(id, frame.clone());
                    }
                    if let Some(entry) = state.get(id) {
                        // 中继广播原样帧；观看端自己按关键帧对齐
                        let _ = entry.tx.send(frame);
                    }
                }
            }
        }
    }
    if let Some(id) = stream_id {
        state.remove(&id);
        tracing::info!("推流结束: {id}");
    }
    let _ = session.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "discovery")]
    fn discovered(ip: &str, port: u16, txt: &[(&str, &str)]) -> crate::discovery::Discovered {
        crate::discovery::Discovered {
            instance: format!("relay-{port}._stross._tcp.local."),
            ip: ip.parse().expect("测试 IP"),
            port,
            txt: txt
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn peers_cache_insert_list_sort() {
        let state = RelayState::default();
        state.insert_peer(PeerInfo {
            id: "10.0.0.2:9000".into(),
            name: "Beta".into(),
            ip: "10.0.0.2".into(),
            port: 9000,
            roles: vec!["relay".into()],
            transports: vec![],
            url: "http://10.0.0.2:9000/".into(),
        });
        state.insert_peer(PeerInfo {
            id: "10.0.0.1:8777".into(),
            name: "Alpha".into(),
            ip: "10.0.0.1".into(),
            port: 8777,
            roles: vec![],
            transports: vec![],
            url: "http://10.0.0.1:8777/".into(),
        });
        let peers = state.peers();
        assert_eq!(peers.len(), 2);
        // 按名称排序：Alpha 在前
        assert_eq!(peers[0].name, "Alpha");
        assert_eq!(peers[1].name, "Beta");
    }

    #[test]
    fn peers_cache_replace_all() {
        let state = RelayState::default();
        let mut map = HashMap::new();
        map.insert(
            "10.0.0.1:8777".into(),
            PeerInfo {
                id: "10.0.0.1:8777".into(),
                name: "A".into(),
                ip: "10.0.0.1".into(),
                port: 8777,
                roles: vec![],
                transports: vec![],
                url: "http://10.0.0.1:8777/".into(),
            },
        );
        state.set_peers(map);
        assert_eq!(state.peers().len(), 1);
        // 整体替换：旧的被清空
        state.set_peers(HashMap::new());
        assert!(state.peers().is_empty());
    }

    #[cfg(feature = "discovery")]
    #[test]
    fn peer_from_discovered_parses_txt() {
        let p = peer_from_discovered(discovered(
            "192.168.1.9",
            8777,
            &[
                ("kind", "relay"),
                ("name", "客厅电脑"),
                ("roles", "sender,viewer,relay"),
                ("transports", "ws,webrtc,srt,quic"),
            ],
        ));
        assert_eq!(p.name, "客厅电脑");
        assert_eq!(p.ip, "192.168.1.9");
        assert_eq!(p.port, 8777);
        assert_eq!(p.roles, vec!["sender", "viewer", "relay"]);
        assert_eq!(p.transports, vec!["ws", "webrtc", "srt", "quic"]);
        assert_eq!(p.url, "http://192.168.1.9:8777/");
    }

    #[cfg(feature = "discovery")]
    #[test]
    fn peer_from_discovered_falls_back_without_txt() {
        let p = peer_from_discovered(discovered("10.0.0.7", 9001, &[]));
        assert_eq!(p.name, "10.0.0.7:9001");
        assert!(p.roles.is_empty());
        assert!(p.transports.is_empty());
    }

    #[cfg(feature = "discovery")]
    #[test]
    fn filter_self_drops_own_broadcast() {
        let self_ip: IpAddr = "192.168.1.5".parse().unwrap();
        let found = vec![
            discovered("192.168.1.5", 8777, &[]), // 自己（本机 IP + 本机端口）
            discovered("192.168.1.5", 9000, &[]), // 本机另一实例（不同端口，保留）
            discovered("192.168.1.8", 8777, &[]), // 其它设备同端口（保留）
            discovered("192.168.1.9", 9001, &[]), // 其它设备（保留）
        ];
        let peers = filter_self(found, 8777, &[self_ip]);
        assert_eq!(peers.len(), 3);
        assert!(peers.contains_key("192.168.1.5:9000"));
        assert!(peers.contains_key("192.168.1.8:8777"));
        assert!(peers.contains_key("192.168.1.9:9001"));
    }
}
