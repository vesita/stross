//! 中继服务器：接收推流，向观看者广播。
//!
//! 借鉴 [MediaMTX](https://github.com/bluenviron/mediamtx) 的"推流端 → 中继 → 观看端"模型：
//!
//! * `GET /ws/push`：推流端 WebSocket（先发 `Hello`，再发二进制媒体帧）
//! * `GET /ws/watch?stream=<id>`：观看端 WebSocket（收到 `Ready` 后收帧）
//! * `GET /api/streams`：流列表（观看端页面拉取）
//! * `GET /`：内嵌的观看端页面
//!
//! 观看端接入时机：视频只在关键帧（IDR）后开始转发（ffmpeg 已在关键帧前重复
//! SPS/PPS，因此等待关键帧即可）；音频 ADTS 自带配置，可直接转发。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::HeaderValue;
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use stross_proto::frame::{FrameHeader, TRACK_VIDEO};
use stross_proto::message::{ControlMessage, StreamInfo};

use crate::assets;

/// 默认中继端口。
pub const DEFAULT_PORT: u16 = 8777;

/// 单条流的内部状态。
#[derive(Clone)]
struct StreamEntry {
    info: StreamInfo,
    tx: broadcast::Sender<Bytes>,
    /// 最近一个视频关键帧（含 SPS/PPS），供新观看者立即对齐 GOP。
    last_keyframe: Option<Bytes>,
}

/// 中继共享状态。
#[derive(Clone, Default)]
pub struct RelayState {
    streams: Arc<Mutex<HashMap<String, StreamEntry>>>,
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
        self.streams.lock().unwrap().insert(entry.info.stream_id.clone(), entry);
    }

    fn set_last_keyframe(&self, id: &str, bytes: Bytes) {
        let mut guard = self.streams.lock().unwrap();
        if let Some(entry) = guard.get_mut(id) {
            entry.last_keyframe = Some(bytes);
        }
    }

    fn remove(&self, id: &str) {
        self.streams.lock().unwrap().remove(id);
    }
}

/// 中继句柄。
pub struct RelayHandle {
    /// 实际监听端口（绑定 0 时由系统分配）。
    pub port: u16,
    state: RelayState,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl RelayHandle {
    /// 当前流列表。
    pub fn streams(&self) -> Vec<StreamInfo> {
        self.state.streams()
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
    /// 返回的 [`RelayHandle::port`] 上。
    pub async fn start(port: u16) -> anyhow::Result<RelayHandle> {
        let state = RelayState::default();
        let app = Router::new()
            .route("/", get(serve_index))
            .route("/style.css", get(serve_style))
            .route("/app.js", get(serve_app_js))
            .route("/jmuxer.js", get(serve_jmuxer))
            .route("/healthz", get(|| async { "ok" }))
            .route("/api/streams", get(api_streams))
            .route("/ws/push", get(ws_push))
            .route("/ws/watch", get(ws_watch))
            .with_state(state.clone());

        let listener = TcpListener::bind(("0.0.0.0", port)).await?;
        let actual_port = listener.local_addr()?.port();
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.changed().await;
                })
                .await;
        });
        tracing::info!("中继已启动: 0.0.0.0:{actual_port}");
        Ok(RelayHandle {
            port: actual_port,
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
    resp.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static(mime));
    resp.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    resp
}

async fn api_streams(State(state): State<RelayState>) -> Json<Vec<StreamInfo>> {
    Json(state.streams())
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
    ws.on_upgrade(move |socket| handle_watch(socket, q.stream, state))
}

async fn ws_push(ws: WebSocketUpgrade, State(state): State<RelayState>) -> Response {
    ws.on_upgrade(move |socket| handle_push(socket, state))
}

/// 观看端：等待 Ready，然后按关键帧对齐转发。
async fn handle_watch(mut ws: WebSocket, stream_id: String, state: RelayState) {
    let Some(entry) = state.get(&stream_id) else {
        let _ = ws
            .send(Message::Text(
                ControlMessage::Error {
                    message: format!("流 {stream_id} 不存在"),
                }
                .to_text()
                .into(),
            ))
            .await;
        return;
    };
    let info = entry.info.clone();
    let mut rx = entry.tx.subscribe();
    let _ = ws
        .send(Message::Text(
            ControlMessage::Ready {
                stream_id: stream_id.clone(),
            }
            .to_text()
            .into(),
        ))
        .await;

    let mut video_started = false;
    // 新观看者先收到最近一个关键帧（含 SPS/PPS），立刻可解码
    if let Some(kf) = entry.last_keyframe.clone() {
        if ws.send(Message::Binary(kf)).await.is_err() {
            return;
        }
        video_started = true;
    }

    loop {
        let bytes = match rx.recv().await {
            Ok(b) => b,
            Err(broadcast::error::RecvError::Lagged(_)) => {
                // 掉帧：重新等下一个关键帧，避免从 GOP 中间开始
                video_started = false;
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        };
        let Ok(header) = FrameHeader::decode(&bytes) else {
            continue;
        };
        if header.track == TRACK_VIDEO && !video_started && !header.is_keyframe() && !header.is_config()
        {
            continue;
        }
        if header.track == TRACK_VIDEO && (header.is_keyframe() || header.is_config()) {
            video_started = true;
        }
        if ws.send(Message::Binary(bytes)).await.is_err() {
            break;
        }
    }
    tracing::debug!("观看端断开: {stream_id} ({})", info.title);
}

/// 推流端：Hello → 建流 → 转发帧；Bye / 断开 → 删流。
async fn handle_push(mut ws: WebSocket, state: RelayState) {
    let mut stream_id: Option<String> = None;
    loop {
        let Some(msg) = ws.next().await else {
            tracing::warn!("推流端连接已断开（无更多消息）");
            break;
        };
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("推流端连接异常: {e}");
                break;
            }
        };
        match msg {
            Message::Text(text) => {
                let Ok(ctrl) = ControlMessage::from_text(&text) else {
                    continue;
                };
                match ctrl {
                    ControlMessage::Hello {
                        stream_id: id,
                        title,
                        video,
                        audio,
                    } => {
                        // 同名流冲突时拒绝新推流端
                        if state.get(&id).is_some() {
                            let _ = ws
                                .send(Message::Text(
                                    ControlMessage::Error {
                                        message: format!("流 {id} 已存在"),
                                    }
                                    .to_text()
                                    .into(),
                                ))
                                .await;
                            let _ = ws.close().await;
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
                        let _ = ws
                            .send(Message::Text(
                                ControlMessage::Welcome { stream_id: id }.to_text().into(),
                            ))
                            .await;
                    }
                    ControlMessage::Bye => {
                        break;
                    }
                    _ => {}
                }
            }
            Message::Binary(bytes) => {
                if let Some(id) = &stream_id {
                    if let Ok(header) = FrameHeader::decode(&bytes) {
                        if header.track == TRACK_VIDEO && header.is_keyframe() {
                            state.set_last_keyframe(id, bytes.clone());
                        }
                        if let Some(entry) = state.get(id) {
                            // 中继广播原样字节；观看端自己按关键帧对齐
                            let _ = entry.tx.send(bytes);
                        }
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    if let Some(id) = stream_id {
        state.remove(&id);
        tracing::info!("推流结束: {id}");
    }
    let _ = ws.close().await;
}
