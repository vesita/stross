//! 中继服务器：接收推流，向观看者广播。
//!
//! 借鉴 [MediaMTX](https://github.com/bluenviron/mediamtx) 的"推流端 → 中继 → 观看端"模型，
//! 并支持**级联代理**（转发链/树）：中继可将上游中继的流拉到本地广播，
//! 观看端只需连接最近的中继（见 [`RelayState::start_proxy`]，对齐 MoQ 的中继链语义）。
//!
//! * `GET /ws/push`：推流端 WebSocket（先发 `Hello`，再发二进制媒体帧）
//! * `GET /ws/watch?stream=<id>`：观看端 WebSocket（收到 `Ready` 后收帧）
//! * `GET /api/streams`：流列表（含本地推流与代理流）
//! * `POST /api/proxy`：请求代理上游中继的流（级联）
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
use stross_proto::message::{ControlMessage, ShareToken, StreamInfo};

use crate::transport::quic::QuicTransport;
use crate::transport::srt::SrtTransport;
use crate::transport::{DataSession, SessionPacket};

/// 默认中继端口。
pub const DEFAULT_PORT: u16 = 8777;

/// 中继数据面事件（内核订阅，用于控制面追踪流生命周期）。
///
/// 对应需求 F2.2「先会话后传输」与 D4「会话 id 内核签发」：受控模式下
/// 只有内核预授权（[`RelayState::authorize_stream`]）或出示有效接入凭证
/// （[`ShareToken`]，经 [`RelayState::set_token_validator`] 注入校验器）的
/// stream_id 才能推流；流的起止 / 观看人数变化通过本事件上报内核。
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

/// 接入凭证校验器（跨设备推流用）。
///
/// 受控中继在预授权之外接受"凭证匹配"：推流端 Hello 携带 [`ShareToken`]，
/// 中继调用本校验器（由内核注入，读取内核签发表）确认凭证有效且
/// `token.stream_id == Hello.stream_id`。默认（未注入）为 `None` = 不接受
/// 凭证接入，行为与现状一致。
pub trait ShareTokenValidator: Send + Sync {
    /// 校验凭证是否有效（存在 / 未过期 / 与签发时一致）。调用方保证
    /// `token.stream_id` 已与 Hello 的 stream_id 比对过。
    fn validate(&self, token: &ShareToken) -> bool;
}

/// 单条流的内部状态。
#[derive(Clone)]
struct StreamEntry {
    info: StreamInfo,
    tx: broadcast::Sender<Frame>,
    /// 最近一个视频关键帧（含 SPS/PPS），供新观看者立即对齐 GOP。
    last_keyframe: Option<Frame>,
}

/// 一条代理流（级联转发）的运行状态。
struct ProxyEntry {
    /// 上游中继基址（`ws://host:port`；`srt://` / `quic://` 亦透传 [`crate::watch::connect_watch`]）。
    upstream: String,
    /// 上游拉取任务句柄（拆除时 abort）。
    task: JoinHandle<()>,
}

/// 流表分片（`Arc` 便于 `RelayState` 克隆共享；每片一把 `Mutex`）。
type StreamShards = Arc<Vec<Arc<Mutex<HashMap<String, StreamEntry>>>>>;

/// 中继共享状态。
#[derive(Clone)]
pub struct RelayState {
    /// 流表分片锁：按 stream_id hash 分片，不同流的转发互不阻塞
    /// （单流内仍原子：last_keyframe 更新 + 广播；多流并发时是真正热路径）。
    streams: StreamShards,
    /// 待完成信令的 WebRTC peer（`/api/webrtc/start` 与 `/answer` 之间）。
    webrtc_peers: Arc<Mutex<HashMap<String, crate::transport::webrtc::WebRtcPeer>>>,
    /// 局域网内其它中继（设备发现缓存；`/api/peers` 返回）。
    peers: Arc<Mutex<HashMap<String, PeerInfo>>>,
    /// 代理流任务表（级联转发；id → 上游 + 拉取任务）。
    /// 代理流同时注册在 [`Self::streams`]，本地观看端走普通转发路径。
    proxies: Arc<Mutex<HashMap<String, ProxyEntry>>>,
    /// 受控模式允许接入的 stream id（内核预注册；非受控模式忽略）。
    allowed: Arc<Mutex<HashSet<String>>>,
    /// 接入凭证校验器（跨设备推流；`None` = 不接受凭证接入）。
    token_validator: Arc<Mutex<Option<Arc<dyn ShareTokenValidator>>>>,
    /// 是否受控：仅允许 [`Self::allowed`] 中的 stream id 推流。
    controlled: bool,
    /// 本机 HTTP/WS 监听端口（`/api/info` 上报用）。
    port: u16,
    /// SRT 推流/观看监听端口（随机分配；`/api/info` 上报用，供前端选 UDP 路径）。
    srt_port: Option<u16>,
    /// QUIC 推流/观看监听端口（随机分配；`/api/info` 上报用）。
    quic_port: Option<u16>,
    /// 数据面事件广播（无人订阅时 send 返回 Err，忽略即可）。
    events: broadcast::Sender<RelayEvent>,
}

/// 流表分片数（幂为 2，取模快；16 片对家庭/小团队并发足够）。
const STREAM_SHARDS: usize = 16;

impl Default for RelayState {
    fn default() -> Self {
        Self {
            streams: Arc::new(
                (0..STREAM_SHARDS)
                    .map(|_| Arc::new(Mutex::new(HashMap::new())))
                    .collect(),
            ),
            webrtc_peers: Arc::new(Mutex::new(HashMap::new())),
            peers: Arc::new(Mutex::new(HashMap::new())),
            proxies: Arc::new(Mutex::new(HashMap::new())),
            allowed: Arc::new(Mutex::new(HashSet::new())),
            token_validator: Arc::new(Mutex::new(None)),
            controlled: false,
            port: 0,
            srt_port: None,
            quic_port: None,
            events: broadcast::channel(64).0,
        }
    }
}

impl RelayState {
    /// stream_id → 分片索引（FNV-1a 变体，够用且无随机 seed 依赖）。
    fn shard_for(id: &str) -> usize {
        let mut h = 0x811c_9dc5u64;
        for b in id.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        (h as usize) & (STREAM_SHARDS - 1)
    }

    /// 流列表（快照；跨分片合并）。
    pub fn streams(&self) -> Vec<StreamInfo> {
        let mut v: Vec<StreamInfo> = Vec::new();
        for shard in self.streams.iter() {
            let guard = shard.lock().unwrap();
            v.extend(guard.values().map(|e| {
                let mut info = e.info.clone();
                info.watchers = e.tx.receiver_count() as u32;
                info
            }));
        }
        v.sort_by(|a, b| a.stream_id.cmp(&b.stream_id));
        v
    }

    fn get(&self, id: &str) -> Option<StreamEntry> {
        self.streams[Self::shard_for(id)]
            .lock()
            .unwrap()
            .get(id)
            .cloned()
    }

    fn insert(&self, entry: StreamEntry) {
        self.streams[Self::shard_for(&entry.info.stream_id)]
            .lock()
            .unwrap()
            .insert(entry.info.stream_id.clone(), entry);
    }

    /// 转发一帧：关键帧时更新缓存，然后广播。
    ///
    /// 热路径：单次加锁（本流分片）完成「缓存更新 + 广播」，
    /// 避免逐帧整体 clone `StreamEntry`；不同流走不同分片锁，互不阻塞。
    fn forward(&self, id: &str, frame: Frame) {
        let mut guard = self.streams[Self::shard_for(id)].lock().unwrap();
        if let Some(entry) = guard.get_mut(id) {
            if frame.header.track == TRACK_VIDEO && frame.header.is_keyframe() {
                entry.last_keyframe = Some(frame.clone());
            }
            let _ = entry.tx.send(frame);
        }
    }

    fn remove(&self, id: &str) -> bool {
        self.streams[Self::shard_for(id)]
            .lock()
            .unwrap()
            .remove(id)
            .is_some()
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

    /// 注入接入凭证校验器（内核调用；`None` 关闭凭证接入，行为与现状一致）。
    pub fn set_token_validator(&self, validator: Option<Arc<dyn ShareTokenValidator>>) {
        *self.token_validator.lock().unwrap() = validator;
    }

    fn is_authorized(&self, id: &str) -> bool {
        self.allowed.lock().unwrap().contains(id)
    }

    /// 凭证接入判定（跨设备推流）：只做凭证校验，**不含预授权**——
    /// 预授权仅对本机（回环来源）放行，见 [`RelayState::is_allowed`]。
    fn token_allows(&self, id: &str, share_token: Option<&str>) -> bool {
        let Some(token_str) = share_token else {
            return false;
        };
        // 凭证解析失败 / 缺失校验器 → 拒绝
        let validator = self.token_validator.lock().unwrap().clone();
        let Some(validator) = validator else {
            return false;
        };
        let Some(token) = ShareToken::from_token_string(token_str) else {
            tracing::warn!("推流被拒绝: 流 {id} 的接入凭证格式非法");
            return false;
        };
        if token.stream_id != id {
            tracing::warn!("推流被拒绝: 流 {id} 的接入凭证 streamId 不匹配");
            return false;
        }
        validator.validate(&token)
    }

    /// 受控模式接入判定（来源感知门控）：
    /// * 回环来源（本机进程）→ 内核预授权放行（本机流程不变）；
    /// * 非回环 / 未知来源（跨设备）→ 必须出示有效接入凭证。
    fn is_allowed(&self, id: &str, share_token: Option<&str>, local: bool) -> bool {
        (local && self.is_authorized(id)) || self.token_allows(id, share_token)
    }

    /// 广播一条数据面事件（无订阅者时忽略）。
    pub fn emit(&self, ev: RelayEvent) {
        let _ = self.events.send(ev);
    }

    /// 订阅数据面事件（内核用）。
    pub fn subscribe_events(&self) -> broadcast::Receiver<RelayEvent> {
        self.events.subscribe()
    }

    /// 在本中继上建立**代理流**（级联转发）：从 `upstream` 中继拉取 `stream_id`，
    /// 作为本地虚拟流广播，实现「观看端 → 本中继 → 上游中继 → 推流端」的转发链/树。
    ///
    /// 代理流对本地观看端完全透明：出现在 `/api/streams`，可被普通 watch 订阅，
    /// 关键帧对齐 / 多观看者广播复用现有路径。上游断开或连接失败时自动清理。
    ///
    /// `info` 可选：调用方已知上游流信息（title/video/audio）时透传，
    /// 避免再向上游查询；缺省用占位信息（解码不受影响——SPS/PPS 随关键帧）。
    pub fn start_proxy(
        &self,
        upstream: &str,
        stream_id: &str,
        info: Option<StreamInfo>,
    ) -> Result<String, String> {
        {
            let mut guards = self.proxies.lock().unwrap();
            if guards.contains_key(stream_id) {
                return Err(format!("本地已有代理流 {stream_id}"));
            }
            if self.get(stream_id).is_some() {
                return Err(format!("本地已有流 {stream_id}（推流或代理）"));
            }
            let (tx, _rx) = broadcast::channel(128);
            let info = info.unwrap_or_else(|| StreamInfo {
                stream_id: stream_id.to_string(),
                title: stream_id.to_string(),
                video: None,
                audio: None,
                started_at: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                watchers: 0,
            });
            // 先注册本地流（观看端立即可见），再启动上游拉取任务
            self.insert(StreamEntry {
                info: info.clone(),
                tx,
                last_keyframe: None,
            });
            let task = tokio::spawn(proxy_uplink(
                self.clone(),
                upstream.to_string(),
                stream_id.to_string(),
            ));
            guards.insert(
                stream_id.to_string(),
                ProxyEntry {
                    upstream: upstream.to_string(),
                    task,
                },
            );
            self.emit(RelayEvent::StreamStarted {
                stream_id: stream_id.to_string(),
                info,
            });
        }
        Ok(stream_id.to_string())
    }

    /// 拆除代理流（上游断开 / 手动调用）：删流 + 上报事件 + 移除任务记录。
    pub fn remove_proxy(&self, id: &str) {
        let removed = self.proxies.lock().unwrap().remove(id).is_some();
        if removed && self.remove(id) {
            self.emit(RelayEvent::StreamEnded {
                stream_id: id.to_string(),
            });
        }
    }

    /// 列出代理流（id → 上游地址），供 `/api/proxies`。
    pub fn proxies(&self) -> Vec<(String, String)> {
        let mut v: Vec<_> = self
            .proxies
            .lock()
            .unwrap()
            .iter()
            .map(|(id, e)| (id.clone(), e.upstream.clone()))
            .collect();
        v.sort();
        v
    }

    /// 拆除全部代理任务（中继停止时调用）。
    pub fn abort_proxies(&self) {
        let tasks: Vec<_> = self
            .proxies
            .lock()
            .unwrap()
            .drain()
            .map(|(_, e)| e.task)
            .collect();
        for t in tasks {
            t.abort();
        }
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

    /// 按媒体类型自动选择推流地址（与接收端 auto 模式同规则）：
    ///
    /// * 含视频 → `srt://host:port`（Adaptive：丢包不阻塞、关键帧自愈）> QUIC > WS
    /// * 纯音频 → `quic://host:port`（无损：音频不可丢）> WS
    ///
    /// `has_video = false` 且无 QUIC 端口时回退 WS；端口缺失时逐级回退。
    /// 返回的是可拨号 URL（WS 带 `/ws/push` 路径，UDP 无路径）。
    pub fn auto_push_url(&self, has_video: bool) -> String {
        if has_video {
            if let Some(p) = self.srt_port {
                return format!("srt://127.0.0.1:{p}");
            }
            if let Some(p) = self.quic_port {
                return format!("quic://127.0.0.1:{p}");
            }
        } else if let Some(p) = self.quic_port {
            return format!("quic://127.0.0.1:{p}");
        }
        format!("ws://127.0.0.1:{}/ws/push", self.port)
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

    /// 停止中继服务（同时拆除全部代理任务）。
    pub async fn stop(self) {
        let _ = self.shutdown.send(true);
        self.state.abort_proxies();
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
        // TCP_NODELAY：媒体每帧一个 WS 消息，Nagle 会叠加延迟（LAN 也受影响）
        let listener = TcpListener::bind(("0.0.0.0", port)).await?.tap_io(|s| {
            let _ = s.set_nodelay(true);
        });
        let actual_port = listener.local_addr()?.port();
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

        // SRT 推流/观看监听：原生端可经 srt://<host>:<srt_port> 推流或观看，
        // 数据面与 WS 完全一致（handle_connect 按首条消息分流 Hello/Watch）
        let mut srt_listener = SrtTransport::new()
            .bind("0.0.0.0:0")
            .await
            .map_err(|e| anyhow::anyhow!("SRT 监听失败: {e}"))?;
        let srt_port = srt_listener.local_addr().port();
        tracing::info!("SRT 推流/观看监听: 0.0.0.0:{srt_port}");

        // QUIC 推流/观看监听：一条连接 control/media 双 stream 多路复用，
        // 原生端可推流（Hello）或观看（Watch）（Lossless；自签名 + 局域网可信模型）
        let mut quic_listener = QuicTransport::new()
            .bind("0.0.0.0:0".parse().expect("静态地址"))
            .await
            .map_err(|e| anyhow::anyhow!("QUIC 监听失败: {e}"))?;
        let quic_port = quic_listener.local_addr().port();
        tracing::info!("QUIC 推流/观看监听: 0.0.0.0:{quic_port}");

        let state = RelayState {
            controlled,
            port: actual_port,
            srt_port: Some(srt_port),
            quic_port: Some(quic_port),
            ..RelayState::default()
        };
        let app = http::router(state.clone());

        let srt_state = state.clone();
        let mut srt_shutdown = shutdown_tx.subscribe();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    res = srt_listener.accept() => match res {
                        Ok(session) => {
                            let state = srt_state.clone();
                            tokio::spawn(async move { handle_connect(session, state).await });
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

        let quic_state = state.clone();
        let mut quic_shutdown = shutdown_tx.subscribe();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    res = quic_listener.accept() => match res {
                        Ok(session) => {
                            let state = quic_state.clone();
                            tokio::spawn(async move { handle_connect(session, state).await });
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
            // ConnectInfo：WS 升级处提取对端地址（来源感知门控用）
            let _ = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
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
    let last_kf = entry.last_keyframe.clone();
    let watchers = entry.tx.receiver_count() as u32;
    // 尽早释放 entry（含 broadcast::Sender 的 clone）：流被删后原始 sender drop，
    // 广播 channel 关闭 → rx.recv() 返回 Closed → 本循环退出并关会话；
    // 否则 sender clone 让 channel 永活，观看会话（含级联代理）悬挂不清理。
    drop(entry);
    state.emit(RelayEvent::WatchersChanged {
        stream_id: stream_id.clone(),
        watchers,
    });
    let _ = session
        .send(SessionPacket::Control(ControlMessage::Ready {
            stream_id: stream_id.clone(),
        }))
        .await;

    let mut video_started = false;
    // 新观看者先收到最近一个关键帧（含 SPS/PPS），立刻可解码
    if let Some(kf) = last_kf {
        if session.send(SessionPacket::Media(kf)).await.is_err() {
            // 会话已死：补发观看数变化，避免计数泄漏
            drop(rx);
            state.emit(RelayEvent::WatchersChanged {
                stream_id: stream_id.clone(),
                watchers: 0,
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
    // 订阅端已断开，广播新的观看者数量（流仍存在时取剩余 receiver 数）
    drop(rx);
    let watchers = state
        .get(&stream_id)
        .map(|e| e.tx.receiver_count() as u32)
        .unwrap_or(0);
    state.emit(RelayEvent::WatchersChanged {
        stream_id: stream_id.clone(),
        watchers,
    });
    tracing::debug!("观看端断开: {stream_id} ({})", info.title);
}

/// 推流端：Hello → 建流 → 转发帧；Bye / 断开 → 删流。
pub(super) async fn handle_push(session: Box<dyn DataSession>, state: RelayState) {
    handle_push_loop(session, state, None).await;
}

/// 统一接入点（SRT/QUIC 监听用）：首条控制消息决定角色——
/// `Hello` = 推流（进入 [`handle_push_loop`]），`Watch` = 观看（[`handle_watch`]）。
pub(super) async fn handle_connect(session: Box<dyn DataSession>, state: RelayState) {
    let first = match session.recv().await {
        Ok(Some(pkt)) => pkt,
        Ok(None) => {
            tracing::warn!("首条消息前连接断开");
            let _ = session.close().await;
            return;
        }
        Err(e) => {
            tracing::warn!("首条消息接收失败: {e}");
            let _ = session.close().await;
            return;
        }
    };
    match first {
        SessionPacket::Control(ControlMessage::Hello { .. }) => {
            handle_push_loop(session, state, Some(first)).await;
        }
        SessionPacket::Control(ControlMessage::Watch { stream_id }) => {
            handle_watch(session, stream_id, state).await;
        }
        other => {
            tracing::warn!("首条消息既不是 Hello 也不是 Watch: {other:?}");
            let _ = session
                .send(SessionPacket::Control(ControlMessage::Error {
                    message: "首条消息必须是 Hello（推流）或 Watch（观看）".into(),
                }))
                .await;
            let _ = session.close().await;
        }
    }
}

/// 推流循环主体；`pending` 为接入点已消费的首包（Hello），WS 路径为 `None`。
async fn handle_push_loop(
    session: Box<dyn DataSession>,
    state: RelayState,
    mut pending: Option<SessionPacket>,
) {
    let mut stream_id: Option<String> = None;
    loop {
        let pkt = match pending.take() {
            Some(pkt) => pkt,
            None => match session.recv().await {
                Ok(Some(pkt)) => pkt,
                Ok(None) => {
                    tracing::warn!("推流端连接已断开（无更多消息）");
                    break;
                }
                Err(e) => {
                    tracing::warn!("推流端连接异常: {e}");
                    break;
                }
            },
        };
        match pkt {
            SessionPacket::Control(ControlMessage::Hello {
                stream_id: id,
                title,
                video,
                audio,
                share_token,
            }) => {
                // 受控模式接入门控（需求 F2.2 + B 阶段凭证式跨设备推流）：
                // 回环来源（本机流程）走内核预授权；非回环 / 未知来源
                // （跨设备，如手机 → 电脑）必须出示有效接入凭证。
                if state.is_controlled() {
                    let local = session
                        .peer_addr()
                        .map(|a| a.ip().is_loopback())
                        .unwrap_or(false);
                    if !state.is_allowed(&id, share_token.as_deref(), local) {
                        tracing::warn!(
                            "推流被拒绝: 流 {id} 来源 {} 未授权（本机需先建会话，跨设备需出示接入凭证）",
                            if local { "回环" } else { "非回环" }
                        );
                        let _ = session
                            .send(SessionPacket::Control(ControlMessage::Error {
                                message: format!("流 {id} 未授权，请先创建会话或出示有效接入凭证"),
                            }))
                            .await;
                        let _ = session.close().await;
                        return;
                    }
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

/// 代理拉取任务：以观看者身份连接上游中继，把收到的帧转发到本地代理流；
/// 上游断开 / 连接失败时清理本地代理流（[`RelayState::remove_proxy`]）。
async fn proxy_uplink(state: RelayState, upstream: String, stream_id: String) {
    let session = match crate::watch::connect_watch(&upstream, &stream_id).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("代理上游连接失败 {upstream} {stream_id}: {e}");
            state.remove_proxy(&stream_id);
            return;
        }
    };
    tracing::info!("代理已连接上游: {upstream} → {stream_id}");
    loop {
        match session.recv().await {
            Ok(Some(SessionPacket::Media(frame))) => state.forward(&stream_id, frame),
            Ok(Some(SessionPacket::Control(_))) => continue,
            Ok(None) => break,
            Err(e) => {
                tracing::warn!("代理上游接收异常: {e}");
                break;
            }
        }
    }
    tracing::info!("代理上游断开，清理本地流: {stream_id}");
    state.remove_proxy(&stream_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use stross_proto::frame::{CODEC_H264, FLAG_KEYFRAME, Frame, TRACK_VIDEO};
    use stross_proto::message::{ControlMessage, ReliabilityProfile};

    use crate::transport::ws::WsTransport;
    use crate::transport::{PeerAddr, SessionParams, Transport};
    use crate::watch::connect_watch;
    use tokio::time::Duration;

    /// 在 `base`（ws://host:port）中继上建流并发送一个关键帧（等 Welcome 确认流已注册）。
    /// 返回推流会话：调用方必须持有它直到断言结束（drop 会断开并删流）。
    async fn push_keyframe(base: &str, stream_id: &str) -> Box<dyn DataSession> {
        let transport = WsTransport::new();
        let peer = PeerAddr {
            transport: stross_proto::message::TransportId::Ws,
            addr: format!("{base}/ws/push"),
        };
        let params = SessionParams {
            session_id: stream_id.into(),
            profile: ReliabilityProfile::Lossless,
        };
        let push = transport.connect(&peer, &params).await.unwrap();
        push.send(SessionPacket::Control(ControlMessage::Hello {
            stream_id: stream_id.into(),
            title: "级联测试流".into(),
            video: None,
            audio: None,
            share_token: None,
        }))
        .await
        .unwrap();
        // 等 Welcome，确保流已建立（避免观看/代理先到报「流不存在」）
        loop {
            match tokio::time::timeout(Duration::from_secs(5), push.recv()).await {
                Ok(Ok(Some(SessionPacket::Control(ControlMessage::Welcome { .. })))) => break,
                Ok(Ok(Some(_))) => continue,
                Ok(Ok(None)) => panic!("推流连接提前关闭"),
                Ok(Err(e)) => panic!("推流 recv 错误: {e}"),
                Err(_) => panic!("等 Welcome 超时"),
            }
        }
        push.send(SessionPacket::Media(Frame::new(
            TRACK_VIDEO,
            CODEC_H264,
            FLAG_KEYFRAME,
            0,
            vec![0x65, 0x88, 0x00, 0x01],
        )))
        .await
        .unwrap();
        push
    }

    /// 从 `base` 中继观看 `stream_id`，直到收到关键帧（带超时）。
    async fn watch_until_keyframe(base: &str, stream_id: &str) {
        let session = connect_watch(base, stream_id)
            .await
            .expect("watch 连接应成功");
        loop {
            match tokio::time::timeout(Duration::from_secs(5), session.recv()).await {
                Ok(Ok(Some(SessionPacket::Media(f)))) if f.header.is_keyframe() => break,
                Ok(Ok(Some(_))) => continue,
                Ok(Ok(None)) => panic!("观看连接提前关闭"),
                Ok(Err(e)) => panic!("观看 recv 错误: {e}"),
                Err(_) => panic!("收关键帧超时"),
            }
        }
    }

    /// 级联拓扑：R1 推流 → R2 代理 R1 的流 → 观看端连 R2 收到关键帧。
    #[tokio::test]
    async fn relay_proxies_remote_stream() {
        let r1 = RelayServer::start(0).await.unwrap();
        let r2 = RelayServer::start(0).await.unwrap();
        let base1 = format!("ws://127.0.0.1:{}", r1.port);
        let base2 = format!("ws://127.0.0.1:{}", r2.port);

        let _push1 = push_keyframe(&base1, "cascade-1").await;

        // R2 代理 R1 的流（调用方未知上游 info，走占位）
        let id = r2.state().start_proxy(&base1, "cascade-1", None).unwrap();
        assert_eq!(id, "cascade-1");
        // 代理流立即出现在 R2 的流列表
        assert!(r2.streams().iter().any(|s| s.stream_id == "cascade-1"));

        // 观看端连 R2 收到关键帧（经 R2 → R1 级联转发）
        watch_until_keyframe(&base2, "cascade-1").await;

        // 代理流信息（/api/streams 展示用）
        let info = r2
            .streams()
            .into_iter()
            .find(|s| s.stream_id == "cascade-1")
            .expect("代理流应存在于 R2 流表");
        assert_eq!(info.title, "cascade-1");

        r1.stop().await;
        r2.stop().await;
    }

    /// 代理不存在的流：任务失败后本地占位流被清理。
    #[tokio::test]
    async fn proxy_of_missing_stream_cleans_up() {
        let r1 = RelayServer::start(0).await.unwrap();
        let r2 = RelayServer::start(0).await.unwrap();
        let base1 = format!("ws://127.0.0.1:{}", r1.port);

        r2.state()
            .start_proxy(&base1, "no-such-stream", None)
            .unwrap();
        // 等拉取任务失败并清理（connect_watch 会收到「流不存在」错误）
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            r2.streams().iter().all(|s| s.stream_id != "no-such-stream"),
            "代理不存在的流应自动清理"
        );

        r1.stop().await;
        r2.stop().await;
    }

    /// 同名冲突：本地已有推流时拒绝代理。
    #[tokio::test]
    async fn proxy_conflicts_with_local_stream() {
        let r2 = RelayServer::start(0).await.unwrap();
        let base1 = format!("ws://127.0.0.1:{}", r2.port);

        let _push2 = push_keyframe(&base1, "local-1").await;
        let err = r2.state().start_proxy("ws://127.0.0.1:1", "local-1", None);
        assert!(err.is_err(), "本地已有同名流时应拒绝代理");

        r2.stop().await;
    }

    /// 上游推流结束后，代理任务退出、本地代理流被清理（覆盖 sender clone 悬挂 bug）。
    #[tokio::test]
    async fn proxy_cleans_up_when_upstream_stream_ends() {
        let r1 = RelayServer::start(0).await.unwrap();
        let r2 = RelayServer::start(0).await.unwrap();
        let base1 = format!("ws://127.0.0.1:{}", r1.port);
        let base2 = format!("ws://127.0.0.1:{}", r2.port);

        let push = push_keyframe(&base1, "short-1").await;
        r2.state().start_proxy(&base1, "short-1", None).unwrap();
        // 确认级联转发已通（观看端经 R2 收到关键帧）
        watch_until_keyframe(&base2, "short-1").await;

        drop(push); // 推流端断开 → R1 删流 → 上游观看会话关闭 → 代理自清理
        // 轮询等待清理（跨进程事件传播需要时间）
        for _ in 0..20 {
            if r2.streams().iter().all(|s| s.stream_id != "short-1") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            r2.streams().iter().all(|s| s.stream_id != "short-1"),
            "上游流结束后代理应自动清理"
        );
        assert!(r2.state().proxies().is_empty(), "代理任务表应清空");

        r1.stop().await;
        r2.stop().await;
    }
}
