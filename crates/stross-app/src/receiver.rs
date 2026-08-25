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

use stross_core::session_channel::{ChannelKind, SessionDataManager};
use stross_core::watch;
use stross_core::SessionPacket;
// 桌面解码播放路径（ffmpeg 子进程）；Android 走 `start_raw` 编码帧转发，
// 由 Kotlin MediaCodec 解码（见 stross-gui `mobile::spawn_android_playback`）。
#[cfg(not(target_os = "android"))]
use stross_media::playback::{
    AudioOut, AudioOutSpec, FfmpegPlaybackSink, PlaybackConfig, PlaybackSession, PlaybackSink,
    VideoOut,
};
use stross_media::playback::RenderedFrame;
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
    #[cfg(not(target_os = "android"))]
    pub async fn start(
        relay_url: String,
        stream_id: String,
        audio_out: AudioOut,
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
        ));
        Ok(Arc::new(Self { inner }))
    }

    /// 开始接收 `relay_url` 上的 `stream_id`，**不解码**：编码帧（Annex-B / ADTS）
    /// 经 [`Receiver::take_raw_frames`] 的通道交给上层。Android 播放路径用
    /// （桌面 PlaybackSink 依赖 ffmpeg 子进程，Android 上由 Kotlin MediaCodec 解码）。
    pub async fn start_raw(relay_url: String, stream_id: String) -> Result<Arc<Self>, String> {
        let (frame_tx, frame_rx) = mpsc::channel::<Frame>(32);
        let inner = Arc::new(ReceiverInner {
            stopped: AtomicBool::new(false),
            stats: Mutex::new(ReceiveStats::default()),
            frames: Mutex::new(None),
            raw_frames: Mutex::new(Some(frame_rx)),
        });
        tokio::spawn(receive_raw_loop(inner.clone(), relay_url, stream_id, frame_tx));
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
) {
    let data = match watch::connect_watch(&relay_url, &stream_id).await {
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
    loop {
        if inner.stopped.load(Ordering::Relaxed) {
            break;
        }
        match data.recv().await {
            Ok(Some(SessionPacket::Media(frame))) => {
                inner.stats.lock().unwrap().received += 1;
                // 单次借用通道：push + poll 共用一个 &mut，避免每帧重复
                // 的 String 分配 + HashMap 查找（热路径）
                let channel = mgr.channel(&stream_id, ChannelKind::Lossless);
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
) {
    let data = match watch::connect_watch(&relay_url, &stream_id).await {
        Ok(d) => d,
        Err(e) => {
            inner.stats.lock().unwrap().error = Some(e);
            return;
        }
    };
    let mut mgr = SessionDataManager::default();
    loop {
        if inner.stopped.load(Ordering::Relaxed) {
            break;
        }
        match data.recv().await {
            Ok(Some(SessionPacket::Media(frame))) => {
                inner.stats.lock().unwrap().received += 1;
                // 单次借用通道（热路径，避免每帧重复 String 分配 + 查找）
                let channel = mgr.channel(&stream_id, ChannelKind::Lossless);
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
