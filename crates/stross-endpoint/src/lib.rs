//! # stross-endpoint —— 数据源/宿插件区（端点层）
//!
//! 本 crate 是 Stross 的**端点插件扩展区**：任何数据源只要实现
//! [`Endpoint`] 契约（端点化 + 数据还原），就能挂载到内核。
//!
//! ## 分层职责（docs/layering-architecture.md）
//!
//! ```text
//! stross-proto     线协议本职：消息/帧/时间 wire 类型、wire 字符串单一真源
//!   └─ stross-endpoint（本 crate）：数据源端点化 + 数据还原
//!        └─ stross-kernel：纯管理调度（会话/鉴权/协商/注册表/路由）
//!             └─ stross-bridge / 壳层
//! ```
//!
//! * **端点化**：每个源自维护「可挂载性」（[`Probe`] load 探测）+「共享」
//!   （[`Endpoint::share`] 启动推送，类型自决，内核不分派）；
//! * **数据还原**：接收侧的解码/播放/落盘与源同处维护——
//!   [`playback`]（画面/声音输出）、[`sink`]（录制/落盘）；
//! * **数据处理辅助**：[`codec`]（H.264 Annex-B / AAC ADTS 切帧）、
//!   [`convert`]（像素格式转换）——源与还原两侧共用。
//!
//! ## 插件区结构（新增数据源 = 加一个目录）
//!
//! * [`screen`]：屏幕源（linux：Wayland portal / X11 x11grab；windows：gdigrab）
//! * [`audio`]：麦克风 / 系统声音源
//! * [`file`]：文件源（确定目标）
//! * 未来源：`message` / `byte_proto` / 游戏联机数据源……
//!
//! ## 采集与还原机制（吸收自原 stross-media）
//!
//! * [`capture`]：采集后端抽象 [`CaptureBackend`] + 桌面 [`FfmpegBackend`]
//!   （ffmpeg 子进程编码；Wayland 屏幕走 [`screen::wayland`] 的
//!   portal+pipewire 采集 → rawvideo 喂 ffmpeg stdin）
//! * [`pipeline`]：ffmpeg 采集与编码管线（[`StreamConfig`] / [`StreamSession`]）
//! * [`devices`]：摄像头 / 麦克风 / 系统声音设备枚举
//! * [`playback`]：播放后端（ffmpeg 子进程解码 + cpal 输出）
//! * [`sink`]：接收/消费侧抽象（[`RecordingSink`] 录制）
//!
//! 依赖方向：本 crate 只依赖 [`stross_proto`] / [`stross_types`] 与通用库，
//! **不依赖内核**——端点经 [`EndpointApp`] 契约调用内核调度能力。

pub mod audio;
pub mod capture;
pub mod codec;
pub mod contract;
pub mod convert;
pub mod devices;
pub mod factory;
pub mod file;
pub mod pipeline;
pub mod playback;
pub mod screen;
pub mod sink;

pub use audio::{MicEndpoint, SystemAudioEndpoint};
pub use capture::{CaptureBackend, CaptureStatus};
pub use codec::adts::AdtsSplitter;
pub use codec::nal::{AccessUnitBuilder, AnnexBSplitter, extract_avc_csd};
pub use contract::{Endpoint, EndpointApp, EndpointBase, Probe, SubscribeCtx, TargetKind};
pub use convert::yuv::{Yuv420Layout, yuv420_to_rgba_scaled};
pub use devices::{CameraDevice, list_audio_inputs, list_cameras, list_system_audio};
pub use factory::{platform_endpoints, seed_platform_endpoints};
pub use file::{FileEndpoint, FilePushOptions};
pub use pipeline::{
    AudioSourceConfig, Quality, StreamConfig, StreamSession, VideoSource, ffmpeg_available,
    ffmpeg_bin,
};
pub use playback::{
    AudioOut, AudioOutSpec, FfmpegPlaybackSink, PlaybackConfig, PlaybackError, PlaybackSession,
    PlaybackSink, PlaybackStats, RenderedFrame, VideoOut,
};
pub use screen::ScreenEndpoint;
pub use sink::{RecordingSink, Sink};
