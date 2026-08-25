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
//!
//! 模块划分：
//!
//! * [`http`]：HTTP 路由 / 静态页面 / REST API / WebSocket 升级 / WebRTC 信令
//! * [`peers`]：局域网设备发现缓存（[`PeerInfo`]，feature `discovery`）

mod http;
mod peers;

pub use peers::PeerInfo;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::serve::{Listener, ListenerExt};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use stross_proto::frame::{Frame, TRACK_VIDEO};
use stross_proto::message::{ControlMessage, StreamInfo};

use crate::transport::quic::QuicTransport;
use crate::transport::srt::SrtTransport;
use crate::transport::{DataSession, SessionPacket};

/// 默认中继端口。
pub const DEFAULT_PORT: u16 = 8777;

/// 中继数据面事件（内核订阅，用于控制面追踪流生命周期）。
///
/// 对应需求 F2.2「先会话后传输」与 D4「会话 id 内核签发」：受控模式下
/// 只有内核预授权（[`RelayState::authorize_stream`]）的 stream_id 才能推流；
/// 流的起止 / 观看人数变化通过本事件上报内核。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum RelayEvent {
    /// 推流端 Hello 成功建流。
    StreamStarted { stream_id: String, info: StreamInfo },
    /// 推流端 Bye / 断开，流被移除。
    StreamEnded { stream_id: String },
    /// 观看者数量变化（订阅 / 断开时上报）。
    WatchersChanged { stream_id: String, watchers: u32 },
}

/// 单条流的内部状态。
#[derive(Clone)]
struct StreamEntry {
    info: StreamInfo,
    tx: broadcast::Sender<Frame>,
    /// 最近一个视频关键帧（含 SPS/PPS），供新观看者立即对齐 GOP。
    last_keyframe: Option<Frame>,
}

/// 中继共享状态。
#[derive(Clone)]
pub struct RelayState {
    streams: Arc<Mutex<HashMap<String, StreamEntry>>>,
    /// 待完成信令的 WebRTC peer（`/api/webrtc/start` 与 `/answer` 之间）。
    webrtc_peers: Arc<Mutex<HashMap<String, crate::transport::webrtc::WebRtcPeer>>>,
    /// 局域网内其它中继（设备发现缓存；`/api/peers` 返回）。
    peers: Arc<Mutex<HashMap<String, PeerInfo>>>,
    /// 受控模式允许接入的 stream id（内核预注册；非受控模式忽略）。
    allowed: Arc<Mutex<HashSet<String>>>,
    /// 是否受控：仅允许 [`Self::allowed`] 中的 stream id 推流。
    controlled: bool,
    /// 数据面事件广播（无人订阅时 send 返回 Err，忽略即可）。
    events: broadcast::Sender<RelayEvent>,
}

impl Default for RelayState {
    fn default() -> Self {
        Self {
            streams: Arc::new(Mutex::new(HashMap::new())),
            webrtc_peers: Arc::new(Mutex::new(HashMap::new())),
            peers: Arc::new(Mutex::new(HashMap::new())),
            allowed: Arc::new(Mutex::new(HashSet::new())),
            controlled: false,
            events: broadcast::channel(64).0,
        }
    }
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

    /// 转发一帧：关键帧时更新缓存，然后广播。
    ///
    /// 热路径：单次加锁完成「缓存更新 + 广播」，避免逐帧整体 clone `StreamEntry`
    /// （旧实现每次 `get` 都会复制 `StreamInfo` 字符串与关键帧 Bytes）。
    fn forward(&self, id: &str, frame: Frame) {
        let mut guard = self.streams.lock().unwrap();
        if let Some(entry) = guard.get_mut(id) {
            if frame.header.track == TRACK_VIDEO && frame.header.is_keyframe() {
                entry.last_keyframe = Some(frame.clone());
            }
            let _ = entry.tx.send(frame);
        }
    }

    fn remove(&self, id: &str) -> bool {
        self.streams.lock().unwrap().remove(id).is_some()
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

    /// 预授权一个 stream id 接入（受控模式下 Hello 校验；非受控模式无效果）。
    pub fn authorize_stream(&self, id: &str) {
        self.allowed.lock().unwrap().insert(id.to_string());
    }

    /// 撤销预授权（会话拆除时调用）。
    ///
    /// 除移除授权外，**同步拆除仍在推送的流**（推流端下次 send 失败即断开）：
    /// 会话拆除 = 数据面流停止，避免"会话已删、媒体仍流转"的泄漏。
    pub fn revoke_stream(&self, id: &str) {
        self.allowed.lock().unwrap().remove(id);
        if self.remove(id) {
            self.emit(RelayEvent::StreamEnded {
                stream_id: id.to_string(),
            });
        }
    }

    /// 是否受控模式。
    pub fn is_controlled(&self) -> bool {
        self.controlled
    }

    fn is_authorized(&self, id: &str) -> bool {
        self.allowed.lock().unwrap().contains(id)
    }

    /// 广播一条数据面事件（无订阅者时忽略）。
    pub fn emit(&self, ev: RelayEvent) {
        let _ = self.events.send(ev);
    }

    /// 订阅数据面事件（内核用）。
    pub fn subscribe_events(&self) -> broadcast::Receiver<RelayEvent> {
        self.events.subscribe()
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

    /// 订阅数据面事件（内核用）。
    pub fn subscribe_events(&self) -> broadcast::Receiver<RelayEvent> {
        self.state.subscribe_events()
    }

    /// 预授权一个 stream id 接入（受控模式）。
    pub fn authorize_stream(&self, id: &str) {
        self.state.authorize_stream(id);
    }

    /// 撤销预授权。
    pub fn revoke_stream(&self, id: &str) {
        self.state.revoke_stream(id);
    }

    /// 是否受控模式（仅授权 id 可推流）。
    pub fn is_controlled(&self) -> bool {
        self.state.is_controlled()
    }

    /// 中继共享状态（克隆句柄，供数据面适配器等共享访问）。
    pub fn state(&self) -> RelayState {
        self.state.clone()
    }

    /// 停止中继服务。
    pub async fn stop(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }
}

impl RelayServer {
    /// 绑定并启动中继（非受控：任意 stream id 可推流，现状行为）。
    ///
    /// `port == 0` 时由系统分配空闲端口（测试用），实际端口在
    /// 返回的 [`RelayHandle::port`] 上；SRT/QUIC 推流监听随机端口，
    /// 见 [`RelayHandle::srt_port`] / [`RelayHandle::quic_port`]。
    pub async fn start(port: u16) -> anyhow::Result<RelayHandle> {
        Self::start_inner(port, false).await
    }

    /// 启动**受控模式**中继：只有 [`RelayHandle::authorize_stream`] 预授权的
    /// stream id 才能推流（对应需求 F2.2「先会话后传输」/ D4「id 内核签发」，
    /// 内嵌中继由内核驱动时使用）。
    pub async fn start_controlled(port: u16) -> anyhow::Result<RelayHandle> {
        Self::start_inner(port, true).await
    }

    async fn start_inner(port: u16, controlled: bool) -> anyhow::Result<RelayHandle> {
        let state = RelayState {
            controlled,
            ..RelayState::default()
        };
        let app = http::router(state.clone());

        // TCP_NODELAY：媒体每帧一个 WS 消息，Nagle 会叠加延迟（LAN 也受影响）
        let listener = TcpListener::bind(("0.0.0.0", port)).await?.tap_io(|s| {
            let _ = s.set_nodelay(true);
        });
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
        peers::spawn_peer_refresh(state.clone(), actual_port, shutdown_tx.subscribe());
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
// 数据面转发（传输无关；HTTP / SRT / QUIC 共用）
// ---------------------------------------------------------------------------

/// 观看端：等待 Ready，然后按关键帧对齐转发。
pub(super) async fn handle_watch(
    session: Box<dyn DataSession>,
    stream_id: String,
    state: RelayState,
) {
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
    state.emit(RelayEvent::WatchersChanged {
        stream_id: stream_id.clone(),
        watchers: entry.tx.receiver_count() as u32,
    });
    let _ = session
        .send(SessionPacket::Control(ControlMessage::Ready {
            stream_id: stream_id.clone(),
        }))
        .await;

    let mut video_started = false;
    // 新观看者先收到最近一个关键帧（含 SPS/PPS），立刻可解码
    if let Some(kf) = entry.last_keyframe.clone() {
        if session.send(SessionPacket::Media(kf)).await.is_err() {
            // 会话已死：补发观看数变化，避免计数泄漏
            drop(rx);
            state.emit(RelayEvent::WatchersChanged {
                stream_id: stream_id.clone(),
                watchers: entry.tx.receiver_count() as u32,
            });
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
    // 订阅端已断开，广播新的观看者数量
    drop(rx);
    state.emit(RelayEvent::WatchersChanged {
        stream_id: stream_id.clone(),
        watchers: entry.tx.receiver_count() as u32,
    });
    tracing::debug!("观看端断开: {stream_id} ({})", info.title);
}

/// 推流端：Hello → 建流 → 转发帧；Bye / 断开 → 删流。
pub(super) async fn handle_push(session: Box<dyn DataSession>, state: RelayState) {
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
                // 受控模式：未预授权的 stream id 拒绝接入（需求 F2.2「先会话后传输」）
                if state.is_controlled() && !state.is_authorized(&id) {
                    tracing::warn!("推流被拒绝: 流 {id} 未授权（请先创建会话）");
                    let _ = session
                        .send(SessionPacket::Control(ControlMessage::Error {
                            message: format!("流 {id} 未授权，请先创建会话"),
                        }))
                        .await;
                    let _ = session.close().await;
                    return;
                }
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
                // 广播容量 128 ≈ 4 秒 @30fps：限制慢观看端的内存积压
                // （超出的观看端收到 Lagged，等下一个关键帧重对齐）
                let (tx, _rx) = broadcast::channel(128);
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
                state.emit(RelayEvent::StreamStarted {
                    stream_id: id.clone(),
                    info: info.clone(),
                });
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
                    // 单次加锁完成关键帧缓存 + 广播（观看端自己按关键帧对齐）
                    state.forward(id, frame);
                }
            }
        }
    }
    if let Some(id) = stream_id {
        // 若流已被 revoke_stream 拆除（会话拆除路径），这里不再重复发事件
        if state.remove(&id) {
            state.emit(RelayEvent::StreamEnded {
                stream_id: id.clone(),
            });
            tracing::info!("推流结束: {id}");
        }
    }
    let _ = session.close().await;
}
