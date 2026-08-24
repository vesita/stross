//! 接收播放引擎（1e）：从局域网中继**接收并原生解码**。
//!
//! 链路：`/ws/watch?stream=`（WS 全序不丢）→ [`SessionDataManager`] 无损通道
//! （1b）→ [`FfmpegPlaybackSink`] 解码（1c，D6）→ 解码帧通道交给上层
//! （GUI 绘制 / 录制）。与发送侧对称，是"接收端有选择权"（F2.1）的实现基础。
//!
//! 音频轨解码但丢弃（`AudioOut::Discard`，无声卡环境可跑），统计块数；
//! 播放到设备由上层按需打开（D3 反向音频路径）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use stross_core::session_channel::{ChannelKind, SessionDataManager};
use stross_media::playback::{
    AudioOut, AudioOutSpec, FfmpegPlaybackSink, PlaybackConfig, PlaybackSession, PlaybackSink,
    RenderedFrame, VideoOut,
};
use stross_proto::frame::Frame;
use stross_proto::message::ControlMessage;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;

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
}

impl Receiver {
    /// 开始接收 `relay_url` 上的 `stream_id`，解码帧经
    /// [`Receiver::take_frames`] 的通道交给上层。
    pub async fn start(relay_url: String, stream_id: String) -> Result<Arc<Self>, String> {
        let (frame_tx, frame_rx) = mpsc::channel::<RenderedFrame>(16);
        let inner = Arc::new(ReceiverInner {
            stopped: AtomicBool::new(false),
            stats: Mutex::new(ReceiveStats::default()),
            frames: Mutex::new(Some(frame_rx)),
        });
        // 播放会话：视频 → 帧通道；音频 → 解码丢弃（统计块数）
        let sink = FfmpegPlaybackSink;
        let session = sink
            .open(PlaybackConfig {
                video: Some(VideoOut { display: None }),
                audio: Some(AudioOutSpec {
                    channels: 2,
                    sample_rate: 48_000,
                    out: AudioOut::Discard,
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

    /// 取出解码帧通道（每会话一次；`None` = 已取过）。
    pub fn take_frames(&self) -> Option<mpsc::Receiver<RenderedFrame>> {
        self.inner.frames.lock().unwrap().take()
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
async fn receive_loop(
    inner: Arc<ReceiverInner>,
    relay_url: String,
    stream_id: String,
    session: PlaybackSession,
    frame_tx: mpsc::Sender<RenderedFrame>,
) {
    let url = format!("{relay_url}/ws/watch?stream={stream_id}");
    let (mut ws, _) = match connect_async(&url).await {
        Ok(v) => v,
        Err(e) => {
            inner.stats.lock().unwrap().error = Some(format!("连接中继失败: {e}"));
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
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            msg = ws.next() => match msg {
                Some(Ok(m)) => {
                    if m.is_text() {
                        let text = m.into_text().unwrap_or_default();
                        if let Ok(ControlMessage::Ready { .. }) = ControlMessage::from_text(&text) {
                            tracing::info!("接收就绪: {stream_id}");
                        }
                    } else if m.is_binary() {
                        let data = m.into_data();
                        if let Ok(frame) = Frame::from_bytes(&data) {
                            inner.stats.lock().unwrap().received += 1;
                            mgr.channel(&stream_id, ChannelKind::Lossless)
                                .push(frame, Instant::now());
                        }
                    }
                }
                Some(Err(_)) | None => break,
            },
        }
        // 通道 → 播放（lossless 直通，按序产出）
        let channel = mgr.channel(&stream_id, ChannelKind::Lossless);
        for f in channel.poll(Instant::now()) {
            if session.push(f).is_err() {
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
