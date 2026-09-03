//! 中继共享状态：流表 / 代理表 / 受控授权 / 事件广播。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use stross_proto::frame::{Frame, TRACK_VIDEO};
use stross_proto::message::{ShareToken, StreamInfo};

use super::peers::PeerInfo;
use crate::kernel::id::Id;
use crate::lock::MutexExt;

/// 中继数据面事件（内核订阅，用于控制面追踪流生命周期）。
///
/// 对应需求 F2.2「先会话后传输」与 D4「会话 id 内核签发」：受控模式下
/// 只有内核预授权（[`RelayState::authorize_stream`]）或出示有效接入凭证
/// （[`ShareToken`]，经 [`RelayState::set_token_validator`] 注入校验器）的
/// stream_id 才能推流；流的起止 / 观看人数变化通过本事件上报内核。
#[derive(Debug, Clone, serde::Serialize)]
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
pub(crate) struct StreamEntry {
    pub(crate) info: StreamInfo,
    pub(crate) tx: broadcast::Sender<Frame>,
    /// 最近一个视频关键帧（含 SPS/PPS），供新观看者立即对齐 GOP。
    pub(crate) last_keyframe: Option<Frame>,
}

/// 一条代理流（级联转发）的运行状态。
struct ProxyEntry {
    /// 上游中继基址（`ws://host:port`；`srt://` / `quic://` 亦透传 [`crate::watch::connect_watch`]）。
    upstream: String,
    /// 上游拉取任务句柄（拆除时 abort）。
    task: JoinHandle<()>,
}

/// 流表分片（`Arc` 便于 `RelayState` 克隆共享；每片一把 `Mutex`）。
type StreamShards = Arc<Vec<Arc<Mutex<HashMap<Id, StreamEntry>>>>>;

/// 流表分片数（幂为 2，取模快；16 片对家庭/小团队并发足够）。
const STREAM_SHARDS: usize = 16;

/// 中继共享状态。
#[derive(Clone)]
pub struct RelayState {
    /// 流表分片锁：按 stream_id hash 分片，不同流的转发互不阻塞
    /// （单流内仍原子：last_keyframe 更新 + 广播；多流并发时是真正热路径）。
    pub(super) streams: StreamShards,
    /// 待完成信令的 WebRTC peer（`/api/webrtc/start` 与 `/answer` 之间）。
    pub(super) webrtc_peers: Arc<Mutex<HashMap<String, crate::transport::webrtc::WebRtcPeer>>>,
    /// 局域网内其它中继（设备发现缓存；`/api/peers` 返回）。
    peers: Arc<Mutex<HashMap<String, PeerInfo>>>,
    /// 代理流任务表（级联转发；id → 上游 + 拉取任务）。
    /// 代理流同时注册在 [`Self::streams`]，本地观看端走普通转发路径。
    proxies: Arc<Mutex<HashMap<String, ProxyEntry>>>,
    /// 受控模式允许接入的 stream id（内核预注册；非受控模式忽略）。
    allowed: Arc<Mutex<HashSet<Id>>>,
    /// 接入凭证校验器（跨设备推流；`None` = 不接受凭证接入）。
    token_validator: Arc<Mutex<Option<Arc<dyn ShareTokenValidator>>>>,
    /// 节点通道管理器（全双工文字与文件互传）。
    channel_manager: Arc<Mutex<Option<Arc<crate::channel::ChannelManager>>>>,
    /// 是否受控：仅允许 [`Self::allowed`] 中的 stream id 推流。
    controlled: bool,
    /// 本机 HTTP/WS 监听端口（`/api/info` 上报用）。
    pub(super) port: u16,
    /// SRT 推流/观看监听端口（随机分配；`/api/info` 上报用，供前端选 UDP 路径）。
    pub(super) srt_port: Option<u16>,
    /// QUIC 推流/观看监听端口（随机分配；`/api/info` 上报用）。
    pub(super) quic_port: Option<u16>,
    /// 数据面事件广播（无人订阅时 send 返回 Err，忽略即可）。
    events: broadcast::Sender<RelayEvent>,
}

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
            channel_manager: Arc::new(Mutex::new(None)),
            controlled: false,
            port: 0,
            srt_port: None,
            quic_port: None,
            events: broadcast::channel(64).0,
        }
    }
}

impl RelayState {
    /// 构造运行时状态（监听端口已确定后由中继启动路径调用）。
    pub(crate) fn with_ports(
        controlled: bool,
        port: u16,
        srt_port: Option<u16>,
        quic_port: Option<u16>,
    ) -> Self {
        Self {
            controlled,
            port,
            srt_port,
            quic_port,
            ..Self::default()
        }
    }

    /// stream_id → 分片索引（FNV-1a 变体，够用且无随机 seed 依赖）。
    fn shard_for(id: &str) -> usize {
        let mut h = 0x811c_9dc5u64;
        for b in id.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        (h as usize) & (STREAM_SHARDS - 1)
    }

    /// 流列表（快照；跨分片合并）。
    pub fn streams(&self) -> Vec<StreamInfo> {
        let mut v: Vec<StreamInfo> = Vec::new();
        for shard in self.streams.iter() {
            let guard = shard.lock_poisoned();
            v.extend(guard.values().map(|e| {
                let mut info = e.info.clone();
                info.watchers = e.tx.receiver_count() as u32;
                info
            }));
        }
        v.sort_by(|a, b| a.stream_id.cmp(&b.stream_id));
        v
    }

    pub(crate) fn get(&self, id: &str) -> Option<StreamEntry> {
        let id = Id::from(id);
        self.streams[Self::shard_for(id.as_str())]
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
    }

    pub(crate) fn insert(&self, entry: StreamEntry) {
        let key = Id::from(entry.info.stream_id.as_str());
        self.streams[Self::shard_for(key.as_str())]
            .lock()
            .unwrap()
            .insert(key, entry);
    }

    /// 转发一帧：关键帧时更新缓存，然后广播。
    ///
    /// 热路径：单次加锁（本流分片）完成「缓存更新 + 广播」，
    /// 避免逐帧整体 clone `StreamEntry`；不同流走不同分片锁，互不阻塞。
    /// `Frame.payload` 为 `Bytes`（原子引用计数），关键帧缓存与广播均为 O(1) 零内存拷贝。
    pub(crate) fn forward(&self, id: &str, frame: Frame) {
        let id = Id::from(id);
        let mut guard = self.streams[Self::shard_for(id.as_str())].lock_poisoned();
        if let Some(entry) = guard.get_mut(&id) {
            if frame.header.track == TRACK_VIDEO && frame.header.is_keyframe() {
                entry.last_keyframe = Some(frame.clone());
            }
            let _ = entry.tx.send(frame);
        }
    }

    pub(crate) fn remove(&self, id: &str) -> bool {
        let id = Id::from(id);
        self.streams[Self::shard_for(id.as_str())]
            .lock()
            .unwrap()
            .remove(&id)
            .is_some()
    }

    /// 局域网设备列表（按名称排序）。
    pub fn peers(&self) -> Vec<PeerInfo> {
        let mut v: Vec<_> = self.peers.lock_poisoned().values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name).then(a.port.cmp(&b.port)));
        v
    }

    /// 整体替换局域网设备表（mDNS 周期浏览结果）。
    pub fn set_peers(&self, peers: HashMap<String, PeerInfo>) {
        *self.peers.lock_poisoned() = peers;
    }

    /// 手动注册一台中继（调试 / 测试 / 手动补充跨网段设备）。
    pub fn insert_peer(&self, peer: PeerInfo) {
        self.peers.lock_poisoned().insert(peer.id.clone(), peer);
    }

    /// 预授权一个 stream id 接入（受控模式下 Hello 校验；非受控模式无效果）。
    pub fn authorize_stream(&self, id: &str) {
        self.allowed.lock_poisoned().insert(Id::from(id));
    }

    /// 撤销预授权（会话拆除时调用）。
    ///
    /// 除移除授权外，**同步拆除仍在推送的流**（推流端下次 send 失败即断开）：
    /// 会话拆除 = 数据面流停止，避免"会话已删、媒体仍流转"的泄漏。
    pub fn revoke_stream(&self, id: &str) {
        self.allowed.lock_poisoned().remove(&Id::from(id));
        if self.remove(id) {
            self.emit(RelayEvent::StreamEnded {
                stream_id: id.to_string(),
            });
        }
    }

    /// 是否受控模式。
    pub const fn is_controlled(&self) -> bool {
        self.controlled
    }

    /// 注入接入凭证校验器（内核调用；`None` 关闭凭证接入，行为与现状一致）。
    pub fn set_token_validator(&self, validator: Option<Arc<dyn ShareTokenValidator>>) {
        *self.token_validator.lock_poisoned() = validator;
    }

    /// 注入节点通道管理器（全双工文字与文件互传）。
    pub fn set_channel_manager(&self, mgr: Arc<crate::channel::ChannelManager>) {
        *self.channel_manager.lock_poisoned() = Some(mgr);
    }

    /// 获取通道管理器引用。
    pub fn channel_manager(&self) -> Option<Arc<crate::channel::ChannelManager>> {
        self.channel_manager.lock_poisoned().clone()
    }

    fn is_authorized(&self, id: &str) -> bool {
        self.allowed.lock_poisoned().contains(&Id::from(id))
    }

    /// 凭证接入判定（跨设备推流）：只做凭证校验，**不含预授权**——
    /// 预授权仅对本机（回环来源）放行，见 [`RelayState::is_allowed`]。
    fn token_allows(&self, id: &str, share_token: Option<&str>) -> bool {
        let Some(token_str) = share_token else {
            return false;
        };
        // 凭证解析失败 / 缺失校验器 → 拒绝
        let validator = self.token_validator.lock_poisoned().clone();
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
    pub(crate) fn is_allowed(&self, id: &str, share_token: Option<&str>, local: bool) -> bool {
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
    ) -> Result<String, crate::error::RelayOpError> {
        {
            let mut guards = self.proxies.lock_poisoned();
            if guards.contains_key(stream_id) {
                return Err(crate::error::RelayOpError::ProxyExists(
                    stream_id.to_string(),
                ));
            }
            if self.get(stream_id).is_some() {
                return Err(crate::error::RelayOpError::StreamExists(
                    stream_id.to_string(),
                ));
            }
            let (tx, _rx) = broadcast::channel(128);
            let info = info.unwrap_or_else(|| StreamInfo {
                stream_id: stream_id.to_string(),
                title: stream_id.to_string(),
                video: None,
                audio: None,
                started_at: stross_proto::time::unix_secs(),
                watchers: 0,
            });
            // 先注册本地流（观看端立即可见），再启动上游拉取任务
            self.insert(StreamEntry {
                info: info.clone(),
                tx,
                last_keyframe: None,
            });
            let task = tokio::spawn(super::data_plane::proxy_uplink(
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
        let removed = self.proxies.lock_poisoned().remove(id).is_some();
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
