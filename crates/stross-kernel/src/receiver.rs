//! 接收播放引擎（1e）：从局域网中继**接收并原生解码**。
//!
//! 链路：`watch`（WS / SRT / QUIC，按 relay URL scheme 选传输，见
//! [`crate::watch::connect_watch`]）→ pick 规则解读模块（[`stross_pick`]）
//! （1b）→ [`FfmpegPlaybackSink`] 解码（1c，D6）→ 解码帧通道交给上层
//! （GUI 绘制 / 录制）。与发送侧对称，是"接收端有选择权"（F2.1）的实现基础。
//!
//! 音频轨解码后输出到设备（`AudioOut::Device`，D3 反向音频路径：电脑扬声器播手机
//! 麦克风）或丢弃（`AudioOut::Discard`，无声卡环境可跑），统计块数。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::SessionPacket;
use crate::relay::RelayState;
use crate::watch;
use stross_pick::InterpretRegistry;
use stross_proto::message::StreamId;
// 桌面解码播放路径（ffmpeg 子进程）；Android 走 `start_raw` 编码帧转发，
// 由 Kotlin MediaCodec 解码（见 stross-gui `mobile::spawn_android_playback`）。
use stross_endpoint::playback::RenderedFrame;
#[cfg(not(target_os = "android"))]
use stross_endpoint::playback::{
    AudioOut, AudioOutSpec, FfmpegPlaybackSink, PlaybackConfig, PlaybackSession, PlaybackSink,
    VideoOut, VideoPacing,
};
use stross_proto::frame::Frame;
use stross_proto::message::PickRule;
use tokio::sync::mpsc;

use crate::error::{Error, Result};

use crate::lock::MutexExt;

/// 接收统计（§7.1 类型去重：单一真源在 [`stross_view::ReceiveStats`]，本文件
/// 旧定义已删除——kernel 统一引用展示视图类型，壳层只读）。
pub use stross_view::ReceiveStats;

/// 「本机中继的代理能力：直连锚点失败时，经它级联拉流（跨网段/防火墙兜底）。
///
/// 由 `Kernel` 在持有本机 `RelayHandle` 时构造传入；无本机中继则为 `None`
/// （降级不可用，直连失败即报错）。
#[derive(Clone)]
pub struct LocalProxy {
    /// 本机中继共享状态（进程内直接调 `start_proxy`，不经 HTTP）。
    pub state: RelayState,
    /// 本机中继 WS 基址（`ws://127.0.0.1:<port>`，代理建立后 watch 它）。
    pub ws_base: String,
}

/// 预留的**单链路接收槽** id：兼容旧单流 API（[`crate::Kernel::start_receive`] /
/// `stop_receive` / `receive_status` / `take_receive_frames`）与 Android 单链
/// 播放路径——它们统一落到这个固定槽位，多链路 API（`start_receive_link` 等）
/// 用自定义 link_id 并存。
pub const MAIN_RECEIVE_LINK: &str = "main";

/// 一条接收链路的状态视图（多链路 API 返回；GUI 面板逐条展示）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceiveLinkView {
    /// 链路 id（上层自定义；`main` 为旧单流兼容槽）。
    pub link_id: String,
    /// 该链路接收统计。
    pub stats: ReceiveStats,
}

/// 按 relay URL scheme 的传输可靠性契约分流（SRT = Adaptive → 有损路径进
/// 抖动缓冲；WS/QUIC = Lossless → 直通零延迟）。推流端 `RelayClient` /
/// 观看端 `connect_watch` 的同一 scheme 判断见 [`crate::transport::transport_for_url`]。
///
/// 本判断留在内核（stross-pick 依赖方向只有 proto，不依赖 transport）：
/// 调用方自行计算 [`ChannelKind`](stross_pick::ChannelKind) 后传给
/// stross-pick 的解读模块。
fn channel_kind_for_url(relay_url: &str) -> stross_pick::ChannelKind {
    match crate::transport::transport_for_url(relay_url).profile() {
        stross_proto::message::ReliabilityProfile::Adaptive => stross_pick::ChannelKind::Lossy,
        _ => stross_pick::ChannelKind::Lossless,
    }
}

/// watch 主循环公共核心：连接（含级联兜底）→ 每帧经解读模块 →
/// `consume` 消费；`sync` 每 100ms 与收尾时同步统计。
/// [`receive_loop`]（解码播放）与 [`receive_raw_loop`]（编码帧转发）共用，
/// 消除两处 ~40 行重复的循环/通道/统计骨架。
async fn watch_consume_loop<C, S>(
    inner: Arc<ReceiverInner>,
    relay_url: String,
    stream_id: String,
    pick_rule: PickRule,
    local_proxy: Option<LocalProxy>,
    consume: C,
    sync: S,
) where
    C: FnMut(Frame) + Send + 'static,
    S: FnMut() + Send + 'static,
{
    let data = match connect_with_proxy(&relay_url, &stream_id, local_proxy.as_ref()).await {
        Ok(d) => d,
        Err(e) => {
            let mut st = inner.stats.lock_poisoned();
            st.error = Some(e.to_user_string());
            st.running = false;
            return;
        }
    };
    watch_consume_loop_connected(
        inner, data, &relay_url, &stream_id, pick_rule, consume, sync,
    )
    .await;
}

/// [`watch_consume_loop`] 的**已连接**版本：连接（含级联兜底）由调用方完成，
/// 本函数只跑消费主循环。`start_recording` 用它把「连接」提前到 ffmpeg 预热
/// 之前（预热期到达的帧缓存在 `data` 通道，headless 全量回放不丢帧）。
async fn watch_consume_loop_connected<C, S>(
    inner: Arc<ReceiverInner>,
    data: Box<dyn crate::DataSession>,
    relay_url: &str,
    stream_id: &str,
    pick_rule: PickRule,
    mut consume: C,
    mut sync: S,
) where
    C: FnMut(Frame) + Send + 'static,
    S: FnMut() + Send + 'static,
{
    // 连接成功即置运行态（不能等首帧或 100ms sync：首帧早到时前端
    // 可能已轮询到 running=false 而误判「流已结束」）
    inner.stats.lock_poisoned().running = true;
    let mut mgr = InterpretRegistry::default();
    // 通道按传输可靠性分流（B5）：SRT（Adaptive，ARQ 超时即丢/可能乱序）→
    // 有损路径进抖动缓冲；WS/QUIC（全序不丢）→ 直通。
    let channel_kind = channel_kind_for_url(relay_url);
    // 强类型流 id（注册表键；热路径只转换一次）
    let stream_key = StreamId::from(stream_id);
    // 统计低频同步（热路径只做帧转发；查询/锁每 100ms 一次）
    let mut last_sync = Instant::now();
    loop {
        if inner.stopped.load(Ordering::Relaxed) {
            break;
        }
        match data.recv().await {
            Ok(Some(SessionPacket::Media(frame))) => {
                inner.received.fetch_add(1, Ordering::Relaxed);
                // 单次借用通道：push + poll 共用一个 &mut（热路径）
                let adapter = mgr.adapter(&stream_key, pick_rule, channel_kind);
                adapter.push(frame);
                // 消息驱动：立即产出
                while let Some(f) = adapter.poll() {
                    consume(f);
                }
            }
            Ok(Some(SessionPacket::Control(_))) => {}
            Ok(None) => break,
            Err(e) => {
                tracing::warn!("观看连接异常: {e}");
                break;
            }
        }
        if last_sync.elapsed() >= Duration::from_millis(100) {
            sync();
            last_sync = Instant::now();
        }
    }
    sync(); // 最终同步
    inner.stats.lock_poisoned().running = false;
}

/// 观看连接：先直连 `relay_url`；失败且提供 `local_proxy` 时，
/// 经本机中继级联代理（`POST /api/proxy` 的进程内等价），再 watch 本地代理流。
async fn connect_with_proxy(
    relay_url: &str,
    stream_id: &str,
    local_proxy: Option<&LocalProxy>,
) -> Result<Box<dyn crate::DataSession>> {
    match watch::connect_watch(relay_url, stream_id).await {
        Ok(d) => Ok(d),
        Err(direct_err) => {
            let Some(proxy) = local_proxy else {
                return Err(Error::Link(direct_err.to_string()));
            };
            tracing::warn!(
                "直连观看失败（{direct_err}），尝试经本机中继级联代理: {relay_url} → {stream_id}"
            );
            if let Err(e) = proxy.state.start_proxy(relay_url, stream_id, None) {
                return Err(Error::Link(format!(
                    "直连失败: {direct_err}；本机代理失败: {e}"
                )));
            }
            watch::connect_watch(&proxy.ws_base, stream_id)
                .await
                .map_err(|e| {
                    Error::Link(format!("直连失败: {direct_err}；代理建立后观看失败: {e}"))
                })
        }
    }
}

/// 一次接收会话。
pub struct Receiver {
    inner: Arc<ReceiverInner>,
}

struct ReceiverInner {
    stopped: AtomicBool,
    received: AtomicU64,
    stats: Mutex<ReceiveStats>,
    /// 协商定稿的解读档案（通信模式 v2）：装载对应解读模块
    /// （RealtimePacing / StrictOrdered）。
    pick_rule: PickRule,
    frames: Mutex<Option<mpsc::Receiver<RenderedFrame>>>,
    /// 编码帧转发通道（`start_raw` 用；Android 播放路径，Kotlin MediaCodec 解码）。
    raw_frames: Mutex<Option<mpsc::Receiver<Frame>>>,
}

impl Receiver {
    /// 开始接收 `relay_url` 上的 `stream_id`，解码帧经
    /// [`Receiver::take_frames`] 的通道交给上层。
    ///
    /// `audio_out` 决定音频去向：设备（扬声器，D3 反向音频）或丢弃。
    /// `local_proxy`：本机中继代理能力（直连失败时级联兜底，见 [`LocalProxy`]）。
    /// `pick_rule`：协商定稿的解读档案（默认 [`PickRule::Realtime`]；
    /// 文件等确定目标订阅方应传 [`PickRule::StrictOrdered`]）。
    #[cfg(not(target_os = "android"))]
    pub async fn start(
        relay_url: String,
        stream_id: String,
        audio_out: AudioOut,
        local_proxy: Option<LocalProxy>,
    ) -> Result<Arc<Self>> {
        Self::start_impl(
            relay_url,
            stream_id,
            audio_out,
            local_proxy,
            false,
            PickRule::Realtime,
        )
        .await
    }

    /// 同 [`Receiver::start`]，可指定解读档案（通信模式 v2：内核按档案
    /// 装载对应解读模块）。
    #[cfg(not(target_os = "android"))]
    pub async fn start_with_rule(
        relay_url: String,
        stream_id: String,
        audio_out: AudioOut,
        local_proxy: Option<LocalProxy>,
        pick_rule: PickRule,
    ) -> Result<Arc<Self>> {
        Self::start_impl(
            relay_url,
            stream_id,
            audio_out,
            local_proxy,
            false,
            pick_rule,
        )
        .await
    }

    /// 开始接收 `relay_url` 上的 `stream_id`，解码帧**全量**经
    /// [`Receiver::take_frames`] 的通道交给上层（消费者慢时阻塞等待、不丢帧）。
    ///
    /// 与 [`Receiver::start`] 的差别在两点（均为 headless 录制语义）：
    /// * **帧保真**：`start` 面向实时显示（解码跟不上时可跳帧，`try_send` 丢帧）；
    ///   `start_recording` 面向落盘/统计（CLI `receive` headless、录制），
    ///   丢一帧都算数据损失；
    /// * **启动时序**：先建立观看连接（失败**同步**返回），再在后台预热 ffmpeg
    ///   播放会话——预热期到达的帧缓存在连接通道内，全量回放
    ///   （`start` 先预热后连接，预热窗口的流会损失）。
    ///
    /// `audio_out` / `local_proxy` 语义同 [`Receiver::start`]。
    #[cfg(not(target_os = "android"))]
    pub async fn start_recording(
        relay_url: String,
        stream_id: String,
        audio_out: AudioOut,
        local_proxy: Option<LocalProxy>,
    ) -> Result<Arc<Self>> {
        // 1) 先连接（失败同步报错，CLI 可 fail-fast）
        let data = connect_with_proxy(&relay_url, &stream_id, local_proxy.as_ref()).await?;
        // 2) 再建通道（open 播放会话在后台进行，预热期帧缓存在 data 通道）
        let (frame_tx, frame_rx) = mpsc::channel::<RenderedFrame>(128);
        let inner = Arc::new(ReceiverInner {
            stopped: AtomicBool::new(false),
            received: AtomicU64::new(0),
            stats: Mutex::new(ReceiveStats::default()),
            pick_rule: PickRule::Realtime,
            frames: Mutex::new(Some(frame_rx)),
            raw_frames: Mutex::new(None),
        });
        tokio::spawn(receive_loop_recording(
            inner.clone(),
            data,
            relay_url,
            stream_id,
            audio_out,
            frame_tx,
            FfmpegPlaybackSink,
        ));
        Ok(Arc::new(Self { inner }))
    }

    #[cfg(not(target_os = "android"))]
    async fn start_impl(
        relay_url: String,
        stream_id: String,
        audio_out: AudioOut,
        local_proxy: Option<LocalProxy>,
        full_frames: bool,
        pick_rule: PickRule,
    ) -> Result<Arc<Self>> {
        let (frame_tx, frame_rx) = mpsc::channel::<RenderedFrame>(128);
        let inner = Arc::new(ReceiverInner {
            stopped: AtomicBool::new(false),
            received: AtomicU64::new(0),
            stats: Mutex::new(ReceiveStats::default()),
            pick_rule,
            frames: Mutex::new(Some(frame_rx)),
            raw_frames: Mutex::new(None),
        });
        // 播放会话：视频 → 帧通道；音频 → 设备播放或丢弃（统计块数）
        let sink = FfmpegPlaybackSink;
        let session = sink
            .open(PlaybackConfig {
                video: Some(VideoOut { display: None }),
                audio: Some(AudioOutSpec {
                    channels: 2,
                    sample_rate: 48_000,
                    out: audio_out,
                }),
                // 实时显示路径：PTS 驱动调度（显示节奏平滑；录制路径不启用）
                video_pacing: Some(VideoPacing::default()),
            })
            .map_err(|e| Error::Message(format!("播放会话打开失败: {e}")))?;
        tokio::spawn(receive_loop(
            inner.clone(),
            relay_url,
            stream_id,
            session,
            frame_tx,
            local_proxy,
            full_frames,
        ));
        Ok(Arc::new(Self { inner }))
    }

    /// 开始接收 `relay_url` 上的 `stream_id`，**不解码**：编码帧（Annex-B / ADTS）
    /// 经 [`Receiver::take_raw_frames`] 的通道交给上层。Android 播放路径用
    /// （桌面 PlaybackSink 依赖 ffmpeg 子进程，Android 上由 Kotlin MediaCodec 解码）。
    /// `local_proxy`：本机中继代理能力（直连失败时级联兜底）。
    pub async fn start_raw(
        relay_url: String,
        stream_id: String,
        local_proxy: Option<LocalProxy>,
    ) -> Result<Arc<Self>> {
        let (frame_tx, frame_rx) = mpsc::channel::<Frame>(32);
        let inner = Arc::new(ReceiverInner {
            stopped: AtomicBool::new(false),
            received: AtomicU64::new(0),
            stats: Mutex::new(ReceiveStats::default()),
            pick_rule: PickRule::Realtime,
            frames: Mutex::new(None),
            raw_frames: Mutex::new(Some(frame_rx)),
        });
        tokio::spawn(receive_raw_loop(
            inner.clone(),
            relay_url,
            stream_id,
            frame_tx,
            local_proxy,
        ));
        Ok(Arc::new(Self { inner }))
    }

    /// 取出解码帧通道（每会话一次；`None` = 已取过）。
    pub fn take_frames(&self) -> Option<mpsc::Receiver<RenderedFrame>> {
        self.inner.frames.lock_poisoned().take()
    }

    /// 取出编码帧通道（`start_raw` 会话；每会话一次）。
    pub fn take_raw_frames(&self) -> Option<mpsc::Receiver<Frame>> {
        self.inner.raw_frames.lock_poisoned().take()
    }

    /// 当前统计。
    pub fn stats(&self) -> ReceiveStats {
        let mut st = self.inner.stats.lock_poisoned().clone();
        st.received = self.inner.received.load(Ordering::Relaxed);
        st
    }

    /// Android 播放路径回写：Kotlin `PlaybackPlugin`（MediaCodec）每解码一帧
    /// 回调一次，与桌面解码线程的 `decoded_video` 统计同口径（真机实测：
    /// Android 观看页「解码 N」恒为 0，因解码发生在 Kotlin 侧）。
    pub fn note_decoded_video(&self) {
        let mut st = self.inner.stats.lock_poisoned();
        st.decoded_video += 1;
        st.running = true;
    }

    /// 停止接收（后台线程收尾）。
    pub fn stop(&self) {
        self.inner.stopped.store(true, Ordering::Relaxed);
    }
}

/// 接收主循环：watch 收帧 → 无损通道 → 播放；统计同步到共享状态。
///
/// 消息驱动（每帧到达立即产出送播放，无固定轮询——50ms tick 曾是
/// 端到端延迟的固定上限来源）。
#[cfg(not(target_os = "android"))]
async fn receive_loop(
    inner: Arc<ReceiverInner>,
    relay_url: String,
    stream_id: String,
    session: PlaybackSession,
    frame_tx: mpsc::Sender<RenderedFrame>,
    local_proxy: Option<LocalProxy>,
    full_frames: bool,
) {
    // 解码帧 → 上层通道：`start`（实时显示）消费者慢时丢帧计数、不反压
    // 阻塞解码；`start_recording`（headless 落盘/统计）全量阻塞发送。
    let mut frames_rx = session.take_video_frames().unwrap_or_else(|| {
        // 未配置视频轨（不应发生）：退化为丢弃通道
        let (_t, r) = mpsc::channel(1);
        r
    });
    let fwd = tokio::spawn(async move {
        while let Some(f) = frames_rx.recv().await {
            if full_frames {
                if frame_tx.send(f).await.is_err() {
                    break; // 上层已丢弃通道（会话结束）
                }
            } else if frame_tx.try_send(f).is_err() {
                // 消费者慢：丢帧（显示可跳帧）
            }
        }
    });

    // 公共主循环：帧消费 = 解码播放；周期同步 = 解码统计
    let sink = session.clone();
    let session_stats = session.clone();
    let inner2 = inner.clone();
    let rule = inner.pick_rule;
    watch_consume_loop(
        inner.clone(),
        relay_url,
        stream_id,
        rule,
        local_proxy,
        move |frame| {
            let _ = sink.push(frame);
        },
        move || {
            let s = session_stats.stats();
            let mut st = inner2.stats.lock_poisoned();
            st.decoded_video = s.video_frames_out;
            st.audio_blocks = s.audio_blocks_out;
            st.audio_blocks_in = s.audio_blocks_in;
            st.dropped = s.dropped_push;
            st.paced_dropped = s.paced_dropped;
            st.paced_reanchors = s.paced_reanchors;
            st.paced_held = s.paced_held;
            st.running = true;
        },
    )
    .await;
    session.stop();
    fwd.abort();
    inner.stats.lock_poisoned().running = false;
}

/// [`Receiver::start_recording`] 的后台接收循环：观看连接已由调用方**提前建立**
/// （`data`，见 [`watch_consume_loop_connected`]），这里只做 ffmpeg 预热 +
/// 全量帧转发 + 消费主循环。时序：连接在前 → 预热期到达的帧缓存在连接通道
/// 内全量回放（headless 录制不丢帧；`receive_loop` 是先预热后连接，实时
/// 显示场景的启动帧损失可接受）。
#[cfg(not(target_os = "android"))]
async fn receive_loop_recording(
    inner: Arc<ReceiverInner>,
    data: Box<dyn crate::DataSession>,
    relay_url: String,
    stream_id: String,
    audio_out: AudioOut,
    frame_tx: mpsc::Sender<RenderedFrame>,
    sink: FfmpegPlaybackSink,
) {
    // 连接成功即置运行态（同 receive_loop 的语义：前端/脚本可立即轮询到）
    inner.stats.lock_poisoned().running = true;
    // ffmpeg 预热（后台；失败写入 stats.error，headless 消费端循环首轮可见）
    let session = match sink.open(PlaybackConfig {
        video: Some(VideoOut { display: None }),
        audio: Some(AudioOutSpec {
            channels: 2,
            sample_rate: 48_000,
            out: audio_out,
        }),
        // headless 录制/统计：全量直通，不经过 PTS 调度层
        video_pacing: None,
    }) {
        Ok(s) => s,
        Err(e) => {
            inner.stats.lock_poisoned().error = Some(format!("播放会话打开失败: {e}"));
            inner.stats.lock_poisoned().running = false;
            return;
        }
    };
    // 解码帧 → 上层通道（**全量**：消费者慢时阻塞等待，不丢帧）
    let mut frames_rx = session.take_video_frames().unwrap_or_else(|| {
        // 未配置视频轨（不应发生）：退化为丢弃通道
        let (_t, r) = mpsc::channel(1);
        r
    });
    let fwd = tokio::spawn(async move {
        while let Some(f) = frames_rx.recv().await {
            if frame_tx.send(f).await.is_err() {
                break; // 上层已丢弃通道（会话结束）
            }
        }
    });

    // 消费主循环：帧消费 = 解码播放；周期同步 = 解码统计
    let sink2 = session.clone();
    let session_stats = session.clone();
    let inner2 = inner.clone();
    let rule = inner.pick_rule;
    watch_consume_loop_connected(
        inner.clone(),
        data,
        &relay_url,
        &stream_id,
        rule,
        move |frame| {
            let _ = sink2.push(frame);
        },
        move || {
            let s = session_stats.stats();
            let mut st = inner2.stats.lock_poisoned();
            st.decoded_video = s.video_frames_out;
            st.audio_blocks = s.audio_blocks_out;
            st.audio_blocks_in = s.audio_blocks_in;
            st.dropped = s.dropped_push;
            st.paced_dropped = s.paced_dropped;
            st.paced_reanchors = s.paced_reanchors;
            st.paced_held = s.paced_held;
            st.running = true;
        },
    )
    .await;
    session.stop();
    fwd.abort();
    inner.stats.lock_poisoned().running = false;
}

/// 编码帧转发主循环：watch 收帧 → 无损通道 → 直接转发（不解码）。
///
/// 消息驱动（同 [`receive_loop`]）。
async fn receive_raw_loop(
    inner: Arc<ReceiverInner>,
    relay_url: String,
    stream_id: String,
    frame_tx: mpsc::Sender<Frame>,
    local_proxy: Option<LocalProxy>,
) {
    // 公共主循环：帧消费 = 编码帧转发；周期同步 = running 标记
    let inner2 = inner.clone();
    let inner3 = inner.clone();
    let rule = inner.pick_rule;
    watch_consume_loop(
        inner.clone(),
        relay_url,
        stream_id,
        rule,
        local_proxy,
        move |f| {
            if frame_tx.try_send(f).is_err() {
                inner2.stats.lock_poisoned().dropped += 1;
            }
        },
        move || {
            inner3.stats.lock_poisoned().running = true;
        },
    )
    .await;
    inner.stats.lock_poisoned().running = false;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DataSession;
    use crate::relay::RelayServer;
    use crate::transport::ws::WsTransport;
    use crate::transport::{PeerAddr, SessionParams, Transport};
    use stross_proto::frame::{CODEC_H264, FLAG_KEYFRAME, Frame, TRACK_VIDEO};
    use stross_proto::message::{ControlMessage, ReliabilityProfile};
    use tokio::time::Duration;

    /// 在 `base`（ws://host:port）中继上建流并发送一个关键帧；返回推流会话（调用方持有）。
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
            title: "降级测试流".into(),
            video: None,
            audio: None,
            share_token: None,
        }))
        .await
        .unwrap();
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

    /// 锚点可达时直连成功（不经代理）。
    #[tokio::test]
    async fn direct_connect_wins() {
        let r = RelayServer::start(0).await.unwrap();
        let base = format!("ws://127.0.0.1:{}", r.port);
        let _push = push_keyframe(&base, "direct-1").await;

        let session = connect_with_proxy(&base, "direct-1", None)
            .await
            .expect("直连应成功");
        // 应收到关键帧
        loop {
            match tokio::time::timeout(Duration::from_secs(5), session.recv()).await {
                Ok(Ok(Some(SessionPacket::Media(f)))) if f.header.is_keyframe() => break,
                Ok(Ok(Some(_))) => continue,
                Ok(Ok(None)) => panic!("观看连接提前关闭"),
                Ok(Err(e)) => panic!("观看 recv 错误: {e}"),
                Err(_) => panic!("收关键帧超时"),
            }
        }
        r.stop().await;
    }

    /// 直连失败 + 有本机代理：走级联代理路径，错误信息说明两段失败原因。
    #[tokio::test]
    async fn fallback_proxy_path_reports_clearly() {
        let r = RelayServer::start(0).await.unwrap(); // 作为"本机中继"的代理能力
        let proxy = LocalProxy {
            state: r.state(),
            ws_base: format!("ws://127.0.0.1:{}", r.port),
        };
        // 锚点不可达（无服务端口）→ 直连失败 → 本机代理也连不上 → 错误含两段原因
        let err = match connect_with_proxy("ws://127.0.0.1:1", "no-such", Some(&proxy)).await {
            Err(e) => e.to_user_string(),
            Ok(_) => panic!("不可达锚点应失败"),
        };
        assert!(
            err.contains("直连失败") && err.contains("代理"),
            "错误信息应说明直连失败并走了代理路径: {err}"
        );
        r.stop().await;
    }

    /// 无本机代理时，直连失败返回原始错误（不误导为代理问题）。
    #[tokio::test]
    async fn no_local_proxy_reports_direct_error() {
        let err = match connect_with_proxy("ws://127.0.0.1:1", "no-such", None).await {
            Err(e) => e.to_user_string(),
            Ok(_) => panic!("不可达锚点应失败"),
        };
        assert!(!err.contains("代理"), "无代理时不应提及代理: {err}");
    }
}
