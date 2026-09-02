//! 播放侧（接收 / 输出）抽象：把协议帧流变成画面与声音。
//!
//! 对称于采集侧 [`crate::capture::CaptureBackend`]（采集 = 设备 → 帧；
//! 播放 = 帧 → 设备），见 docs/requirements.md §9 适配层与决策 D6：
//! 平台无关 trait + 每平台实现。桌面实现是 ffmpeg 子进程解码 + cpal 输出
//! （[`FfmpegPlaybackSink`]）；Android（一期 1f）用 MediaCodec + AudioTrack
//! 实现同一 trait。
//!
//! 使用方式：
//!
//! ```ignore
//! let sink: Arc<dyn PlaybackSink> = Arc::new(FfmpegPlaybackSink);
//! let session = sink.open(cfg)?;                 // 启动解码与输出
//! let mut frames = session.take_video_frames();  // 解码画面通道（GUI 消费）
//! loop { session.push(frame)?; }                 // 喂协议帧（同步、非阻塞）
//! session.stop();
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use stross_proto::frame::Frame;
use stross_proto::message::{CapabilityDescriptor, CodecId};

#[cfg(not(target_os = "android"))]
pub mod audio_out;
/// ffmpeg 子进程解码后端：仅桌面（Android 播放走 Kotlin MediaCodec，见
/// `apps/stross-gui/src-tauri/android/PlaybackPlugin.kt`）。
#[cfg(not(target_os = "android"))]
pub mod ffmpeg;
pub mod schedule;

#[cfg(not(target_os = "android"))]
pub use ffmpeg::FfmpegPlaybackSink;

/// 视频输出配置。
#[derive(Debug, Clone, Copy)]
pub struct VideoOut {
    /// 显示目标尺寸（保持宽高比）；`None` = 原始分辨率（GUI 侧再缩放）。
    pub display: Option<(u32, u32)>,
}

/// 音频输出方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioOut {
    /// 默认输出设备（扬声器 / 录音设备，D3 反向麦克风的关键路径）。
    Device,
    /// 解码但丢弃（无声卡环境 / 测试）。
    Discard,
}

/// 音频输出配置。
#[derive(Debug, Clone, Copy)]
pub struct AudioOutSpec {
    /// 声道数（解码输出；设备模式下以设备为准）。
    pub channels: u8,
    /// 采样率（解码输出；设备模式下以设备为准）。
    pub sample_rate: u32,
    /// 输出方式。
    pub out: AudioOut,
}

/// 播放会话配置。
#[derive(Debug, Clone, Copy, Default)]
pub struct PlaybackConfig {
    /// 视频轨（H264）；`None` = 不播放视频。
    pub video: Option<VideoOut>,
    /// 音频轨（AAC）；`None` = 不播放音频。
    pub audio: Option<AudioOutSpec>,
    /// PTS 驱动播放调度（实时显示路径启用；`None` = 直通零延迟——录制/
    /// headless 全量语义，不经过调度层）。
    pub video_pacing: Option<VideoPacing>,
}

/// PTS 驱动播放调度配置。
///
/// 解码帧按源节奏（pts 相对间距）调度输出：首帧到达即锚定播放时钟，
/// 后续帧等到各自 play 时刻再发出——网络抖动被缓冲吸收，显示节奏平滑；
/// 队尾（最新帧）play 时刻晚于「现在 + [`Self::target_delay`]」时丢队尾
/// 追平实时（发送端过快 / 时钟漂移；LAN 下实际缓冲 ≈ 网络抖动，远小于
/// 该值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoPacing {
    /// 目标播放延迟水位：队尾 play 时刻超过该值 → 丢最新帧追平。
    pub target_delay: Duration,
    /// 大 PTS 跳变阈值：相邻锚点偏差超过该值 → 重置缓冲重锚定
    /// （流切换 / 重连 / 失步重建）。
    pub jump_reset: Duration,
}

impl Default for VideoPacing {
    fn default() -> Self {
        Self {
            target_delay: Duration::from_millis(150),
            jump_reset: Duration::from_millis(500),
        }
    }
}

/// 一帧解码后的画面（RGBA8888，可直接交给 GUI 绘制）。
#[derive(Debug)]
pub struct RenderedFrame {
    /// 源帧时间戳（毫秒，来自协议帧头）。
    pub pts_ms: u32,
    pub width: u32,
    pub height: u32,
    /// RGBA8888，长度 = width × height × 4。
    pub rgba: Vec<u8>,
}

/// 播放会话统计（可观测、可测试）。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackStats {
    /// 收到的视频帧数。
    pub video_frames_in: u64,
    /// 解码产出的画面帧数。
    pub video_frames_out: u64,
    /// 视频解码失步重对齐次数（子进程重建）。
    pub video_resyncs: u64,
    /// 收到的音频块数（1 块 = 1 个 ADTS 帧）。
    pub audio_blocks_in: u64,
    /// 解码产出的音频块数。
    pub audio_blocks_out: u64,
    /// 音频输出设备是否可用（false = 静音回退）。
    pub audio_device_ok: bool,
    /// 内部缓冲满被丢弃的帧数（内存有界保障）。
    pub dropped_push: u64,
    /// 调度层：过水位丢帧数（发送过快/时钟漂移时追平到实时）。
    pub paced_dropped: u64,
    /// 调度层：大 PTS 跳变重置锚点次数（流切换 / 重连 / 失步重建）。
    pub paced_reanchors: u64,
    /// 调度层：等待到 play 时刻后按时发出的帧数。
    pub paced_held: u64,
}

/// 播放错误。
#[derive(Debug, thiserror::Error)]
pub enum PlaybackError {
    #[error("未找到 ffmpeg（可设置 STROSS_FFMPEG 环境变量）")]
    NoFfmpeg,
    #[error("不支持的视频编码: {0:?}")]
    UnsupportedVideo(CodecId),
    #[error("不支持的音频编码: {0:?}")]
    UnsupportedAudio(CodecId),
    #[error("播放会话已停止")]
    Closed,
    #[error("启动播放失败: {0}")]
    Spawn(String),
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
}

/// 播放后端：把帧流变成画面与声音。
///
/// 与 [`CaptureBackend`](crate::capture::CaptureBackend) 对称：
/// 同步方法 + `Box<dyn>` 友好，重活都在后台线程。
pub trait PlaybackSink: Send + Sync {
    /// 能力描述（能力广播 / 协商用）。
    fn descriptor(&self) -> CapabilityDescriptor;
    /// 打开一个播放会话（按配置启动解码与输出）。
    fn open(&self, cfg: PlaybackConfig) -> Result<PlaybackSession, PlaybackError>;
}

/// 一次播放会话：`push` 帧 → 后台解码输出。
///
/// * [`PlaybackSession::push`] 是同步廉价入口（有界队列，满则丢弃并计数）；
/// * 视频解码帧经 [`PlaybackSession::take_video_frames`] 的通道交给 GUI；
/// * 音频直接输出到设备（或丢弃），无需上层干预。
#[derive(Clone)]
pub struct PlaybackSession {
    inner: Arc<SessionInner>,
}

pub(crate) struct SessionInner {
    pub(crate) stats: Arc<Mutex<PlaybackStats>>,
    pub(crate) stopped: Arc<AtomicBool>,
    pub(crate) video_tx: Mutex<Option<std::sync::mpsc::SyncSender<Frame>>>,
    pub(crate) audio_tx: Mutex<Option<std::sync::mpsc::SyncSender<Frame>>>,
    pub(crate) video_rx_out: Mutex<Option<tokio::sync::mpsc::Receiver<RenderedFrame>>>,
    /// 视频失步标记（与 writer 线程共享）：丢帧后置位，writer 等关键帧重建，
    /// 避免把花屏帧喂给解码器（与 [`PlaybackStats::dropped_push`] 联动）。
    pub(crate) video_resync: Option<Arc<AtomicBool>>,
    pub(crate) threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

impl PlaybackSession {
    /// 喂入一帧（按轨道分流）。同步、非阻塞；队列满则丢弃并计入
    /// [`PlaybackStats::dropped_push`]（内存有界，不反向阻塞调用方）。
    pub fn push(&self, frame: Frame) -> Result<(), PlaybackError> {
        if self.inner.stopped.load(Ordering::Relaxed) {
            return Err(PlaybackError::Closed);
        }
        use stross_proto::frame::{TRACK_AUDIO, TRACK_VIDEO};
        let is_video = frame.header.track == TRACK_VIDEO;
        // 只锁当前帧轨道对应的发送端：此前每帧同时锁 video+audio 两个互斥量，
        // 是接收侧逐帧 push 热路径上可避免的双锁开销。`guard` 绑定到具名变量
        // 以延长临时守卫生命周期（否则临时守卫在 let 语句末被释放 → E0716）。
        let guard;
        let tx = match frame.header.track {
            TRACK_VIDEO => {
                guard = self.inner.video_tx.lock().unwrap();
                guard.as_ref()
            }
            TRACK_AUDIO => {
                guard = self.inner.audio_tx.lock().unwrap();
                guard.as_ref()
            }
            _ => return Ok(()), // 未知轨道：静默忽略，无需上锁
        };
        // 该轨道未配置发送端（如纯视频会话收到音频帧）：与原来 `None => Ok(())`
        // 一致，静默忽略
        let Some(tx) = tx else { return Ok(()) };
        match tx.try_send(frame) {
            Ok(()) => Ok(()),
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                self.inner.stats.lock().unwrap().dropped_push += 1;
                // 视频丢帧会撕裂解码链 → 置失步，writer 等关键帧重建
                // （H.264 花屏帧不喂解码器，最多一个 GOP 后干净恢复）
                if is_video && let Some(r) = self.inner.video_resync.as_ref() {
                    r.store(true, Ordering::Relaxed);
                }
                Ok(())
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => Err(PlaybackError::Closed),
        }
    }

    /// 取出解码画面通道（每会话一次；`None` = 已取过或未配置视频轨）。
    pub fn take_video_frames(&self) -> Option<tokio::sync::mpsc::Receiver<RenderedFrame>> {
        self.inner.video_rx_out.lock().unwrap().take()
    }

    /// 当前统计。
    pub fn stats(&self) -> PlaybackStats {
        *self.inner.stats.lock().unwrap()
    }

    /// 停止播放并等待后台线程收尾（子进程一并结束）。
    pub fn stop(&self) {
        if self.inner.stopped.swap(true, Ordering::Relaxed) {
            return;
        }
        // 关闭帧入口 → 写线程退出 → 关 stdin → 子进程 EOF 退出
        drop(self.inner.video_tx.lock().unwrap().take());
        drop(self.inner.audio_tx.lock().unwrap().take());
        let threads = std::mem::take(&mut *self.inner.threads.lock().unwrap());
        for t in threads {
            let _ = t.join();
        }
    }
}

impl Drop for SessionInner {
    fn drop(&mut self) {
        // 未显式 stop 就 drop：关闭帧入口，后台线程自行收尾（不 join，进程退出兜底）
        if !self.stopped.swap(true, Ordering::Relaxed) {
            drop(self.video_tx.lock().unwrap().take());
            drop(self.audio_tx.lock().unwrap().take());
        }
    }
}
