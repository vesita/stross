//! # stross-media —— 系统适配模块
//!
//! 负责"把本机媒体源变成协议帧 / 把协议帧变成画面声音"这一层平台适配：
//!
//! * [`devices`]：摄像头 / 麦克风 / 系统声音设备枚举
//! * [`pipeline`]：ffmpeg 采集与编码管线（桌面端），产出 [`stross_proto::frame::Frame`]
//! * [`capture`]：统一的采集后端抽象 [`CaptureBackend`]（Source 能力），
//!   桌面实现是 ffmpeg，Android 由 UI 层用原生 MediaProjection/MediaCodec 实现
//! * [`playback`]：对称的播放后端抽象 [`PlaybackSink`]（Sink 能力，D6），
//!   桌面实现 [`FfmpegPlaybackSink`] 是 ffmpeg 子进程解码 + cpal 输出；
//!   Android（一期 1f）用 MediaCodec + AudioTrack 实现同一 trait
//! * [`sink`]：接收/消费侧抽象 [`Sink`] 与首个实现 [`RecordingSink`]（录制，§6.2）
//! * [`adts`] / [`nal`]：AAC ADTS 与 H.264 Annex-B 流切帧（含 SPS 分辨率解析）
//!
//! 本模块只依赖协议模块 [`stross_proto`]，不依赖任何共享/中继逻辑，
//! 保证"系统适配"是架构里最底层的叶子之一。

pub mod adts;
pub mod capture;
pub mod devices;
pub mod nal;
pub mod pipeline;
pub mod playback;
pub mod sink;
pub mod yuv;

pub use capture::{CaptureBackend, CaptureStatus};
pub use devices::{CameraDevice, list_audio_inputs, list_cameras, list_system_audio};
pub use pipeline::{
    AudioSourceConfig, Quality, StreamConfig, StreamSession, VideoSource, ffmpeg_available,
    ffmpeg_bin,
};
#[cfg(not(target_os = "android"))]
pub use playback::{
    AudioOut, AudioOutSpec, FfmpegPlaybackSink, PlaybackConfig, PlaybackError, PlaybackSession,
    PlaybackSink, PlaybackStats, RenderedFrame, VideoOut,
};
#[cfg(target_os = "android")]
pub use playback::{
    AudioOut, AudioOutSpec, PlaybackConfig, PlaybackError, PlaybackSession, PlaybackSink,
    PlaybackStats, RenderedFrame, VideoOut,
};
pub use sink::{RecordingSink, Sink};
