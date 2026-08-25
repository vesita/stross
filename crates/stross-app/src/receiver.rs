//! 接收播放引擎（1e）：从局域网中继**接收并原生解码**。
//!
//! 链路：`watch`（WS / SRT / QUIC，按 relay URL scheme 选传输，见
//! [`stross_core::watch::connect_watch`]）→ [`SessionDataManager`] 无损通道
//! （1b）→ [`FfmpegPlaybackSink`] 解码（1c，D6）→ 解码帧通道交给上层
//! （GUI 绘制 / 录制）。与发送侧对称，是"接收端有选择权"（F2.1）的实现基础。
//!
//! 音频轨解码后输出到设备（`AudioOut::Device`，D3 反向音频路径：电脑扬声器播手机
//! 麦克风）或丢弃（`AudioOut::Discard`，无声卡环境可跑），统计块数。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use stross_core::SessionPacket;
use stross_core::relay::RelayState;
use stross_core::session_channel::{ChannelKind, SessionDataManager};
use stross_core::watch;
// 桌面解码播放路径（ffmpeg 子进程）；Android 走 `start_raw` 编码帧转发，
// 由 Kotlin MediaCodec 解码（见 stross-gui `mobile::spawn_android_playback`）。
use stross_media::playback::RenderedFrame;
#[cfg(not(target_os = "android"))]
use stross_media::playback::{
    AudioOut, AudioOutSpec, FfmpegPlaybackSink, PlaybackConfig, PlaybackSession, PlaybackSink,
    VideoOut,
};
use stross_proto::frame::Frame;
use tokio::sync::mpsc;

/// 接收统计（可观测、可测试）。
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceiveStats {
    /// 是否在接收中。
    pub running: bool,
    /// 收到的协议帧数。
    pub received: u64,
    /// 解码产出的视频帧数。
    pub decoded_video: u64,
    /// 解码产出的音频块数。
    pub audio_blocks: u64,
    /// 帧通道满被丢弃的帧数（消费者慢）。
    pub dropped: u64,
    /// 失败原因（连接失败 / 流不存在等）。
    pub error: Option<String>,
}

/// 本机中继的代理能力：直连锚点失败时，经它级联拉流（跨网段/防火墙兜底）。
///
/// 由 `StrossApp` 在持有本机 `RelayHandle` 时构造传入；无本机中继则为 `None`
/// （降级不可用，直连失败即报错）。
#[derive(Clone)]
pub struct LocalProxy {
    /// 本机中继共享状态（进程内直接调 `start_proxy`，不经 HTTP）。
    pub state: RelayState,
    /// 本机中继 WS 基址（`ws://127.0.0.1:<port>`，代理建立后 watch 它）。
    pub ws_base: String,
}

/// 按传输 scheme 选通道可靠性（B5）：
/// * `srt://`（Adaptive：ARQ 超时即丢、TSBPD 乱序窗口）→ 有损 → 抖动缓冲；
/// * `ws://` / `quic://`（全序不丢）→ 无损直通（零额外延迟）。
fn channel_kind_for(relay_url: &str) -> ChannelKind {
    if relay_url.starts_with("srt://") {
        ChannelKind::Lossy
    } else {
        ChannelKind::Lossless
    }
}

/// 观看连接：先直连 `relay_url`；失败且提供 `local_proxy` 时，
/// 经本机中继级联代理（`POST /api/proxy` 的进程内等价），再 watch 本地代理流。
async fn connect_with_proxy(
    relay_url: &str,
    stream_id: &str,
    local_proxy: Option<&LocalProxy>,
) -> Result<Box<dyn stross_core::DataSession>, String> {
    match watch::connect_watch(relay_url, stream_id).await {
        Ok(d) => Ok(d),
        Err(direct_err) => {
            let Some(proxy) = local_proxy else {
                return Err(direct_err);
            };
            tracing::warn!(
                "直连观看失败（{direct_err}），尝试经本机中继级联代理: {relay_url} → {stream_id}"
            );
            if let Err(e) = proxy.state.start_proxy(relay_url, stream_id, None) {
                return Err(format!("直连失败: {direct_err}；本机代理失败: {e}"));
            }
            watch::connect_watch(&proxy.ws_base, stream_id)
                .await
                .map_err(|e| format!("直连失败: {direct_err}；代理建立后观看失败: {e}"))
        }
    }
}

/// 一次接收会话。
pub struct Receiver {
    inner: Arc<ReceiverInner>,
}

struct ReceiverInner {
    stopped: AtomicBool,
    stats: Mutex<ReceiveStats>,
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
    #[cfg(not(target_os = "android"))]
    pub async fn start(
        relay_url: String,
        stream_id: String,
        audio_out: AudioOut,
        local_proxy: Option<LocalProxy>,
    ) -> Result<Arc<Self>, String> {
        let (frame_tx, frame_rx) = mpsc::channel::<RenderedFrame>(16);
        let inner = Arc::new(ReceiverInner {
            stopped: AtomicBool::new(false),
            stats: Mutex::new(ReceiveStats::default()),
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
            })
            .map_err(|e| e.to_string())?;
        tokio::spawn(receive_loop(
            inner.clone(),
            relay_url,
            stream_id,
            session,
            frame_tx,
            local_proxy,
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
    ) -> Result<Arc<Self>, String> {
        let (frame_tx, frame_rx) = mpsc::channel::<Frame>(32);
        let inner = Arc::new(ReceiverInner {
            stopped: AtomicBool::new(false),
            stats: Mutex::new(ReceiveStats::default()),
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
        self.inner.frames.lock().unwrap().take()
    }

    /// 取出编码帧通道（`start_raw` 会话；每会话一次）。
    pub fn take_raw_frames(&self) -> Option<mpsc::Receiver<Frame>> {
        self.inner.raw_frames.lock().unwrap().take()
    }

    /// 当前统计。
    pub fn stats(&self) -> ReceiveStats {
        self.inner.stats.lock().unwrap().clone()
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
) {
    let data = match connect_with_proxy(&relay_url, &stream_id, local_proxy.as_ref()).await {
        Ok(d) => d,
        Err(e) => {
            inner.stats.lock().unwrap().error = Some(e);
            return;
        }
    };
    // 解码帧 → 上层通道（消费者慢则丢帧计数，不反压阻塞解码）
    let mut frames_rx = session.take_video_frames().unwrap_or_else(|| {
        // 未配置视频轨（不应发生）：退化为丢弃通道
        let (_t, r) = mpsc::channel(1);
        r
    });
    let fwd = tokio::spawn(async move {
        while let Some(f) = frames_rx.recv().await {
            if frame_tx.try_send(f).is_err() {
                // 消费者慢：丢帧（显示可跳帧）
            }
        }
    });

    let mut mgr = SessionDataManager::default();
    // 通道按传输可靠性分流（B5）：SRT（Adaptive，ARQ 超时即丢、可能乱序）→
    // 有损路径进抖动缓冲；WS/QUIC（全序不丢）→ 直通。
    let channel_kind = channel_kind_for(&relay_url);
    loop {
        if inner.stopped.load(Ordering::Relaxed) {
            break;
        }
        match data.recv().await {
            Ok(Some(SessionPacket::Media(frame))) => {
                inner.stats.lock().unwrap().received += 1;
                // 单次借用通道：push + poll 共用一个 &mut，避免每帧重复
                // 的 String 分配 + HashMap 查找（热路径）
                let channel = mgr.channel(&stream_id, channel_kind);
                channel.push(frame, Instant::now());
                // 消息驱动：立即产出送播放
                for f in channel.poll(Instant::now()) {
                    if session.push(f).is_err() {
                        break;
                    }
                }
            }
            Ok(Some(SessionPacket::Control(_))) => {}
            Ok(None) => break,
            Err(e) => {
                tracing::warn!("观看连接异常: {e}");
                break;
            }
        }
        // 同步解码统计
        let s = session.stats();
        let mut st = inner.stats.lock().unwrap();
        st.decoded_video = s.video_frames_out;
        st.audio_blocks = s.audio_blocks_out;
        st.dropped = s.dropped_push;
        st.running = true;
    }
    session.stop();
    fwd.abort();
    inner.stats.lock().unwrap().running = false;
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
    let data = match connect_with_proxy(&relay_url, &stream_id, local_proxy.as_ref()).await {
        Ok(d) => d,
        Err(e) => {
            inner.stats.lock().unwrap().error = Some(e);
            return;
        }
    };
    let mut mgr = SessionDataManager::default();
    // 同 [`receive_loop`]：按传输可靠性分流（SRT 有损 → 抖动缓冲）
    let channel_kind = channel_kind_for(&relay_url);
    loop {
        if inner.stopped.load(Ordering::Relaxed) {
            break;
        }
        match data.recv().await {
            Ok(Some(SessionPacket::Media(frame))) => {
                inner.stats.lock().unwrap().received += 1;
                // 单次借用通道（热路径，避免每帧重复 String 分配 + 查找）
                let channel = mgr.channel(&stream_id, channel_kind);
                channel.push(frame, Instant::now());
                // 通道 → 转发（lossless 直通，按序产出；消费者慢则丢帧，不反压）
                for f in channel.poll(Instant::now()) {
                    if frame_tx.try_send(f).is_err() {
                        inner.stats.lock().unwrap().dropped += 1;
                    }
                }
            }
            Ok(Some(SessionPacket::Control(_))) => {}
            Ok(None) => break,
            Err(e) => {
                tracing::warn!("观看连接异常: {e}");
                break;
            }
        }
        inner.stats.lock().unwrap().running = true;
    }
    inner.stats.lock().unwrap().running = false;
}

#[cfg(test)]
mod tests {
    use super::*;
    use stross_core::DataSession;
    use stross_core::relay::RelayServer;
    use stross_core::transport::ws::WsTransport;
    use stross_core::transport::{PeerAddr, SessionParams, Transport};
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
            Err(e) => e,
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
            Err(e) => e,
            Ok(_) => panic!("不可达锚点应失败"),
        };
        assert!(!err.contains("代理"), "无代理时不应提及代理: {err}");
    }
}
