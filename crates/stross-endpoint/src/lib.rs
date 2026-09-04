//! # stross-endpoint —— 端点插件区（分享端 / 订阅端）
//!
//! v3（docs/framework-v3.md §3.2）：本 crate 是**端点概念 crate（契约 + 实现
//! 同仓）**：实现本 crate 的契约特性（[`ShareEndpoint`] / [`SubscribeEndpoint`]），
//! 即可挂载到内核——**内核约定特性、端点实现、内核只基于特性行动**。
//!
//! ## 分层职责（docs/framework-v3.md §2）
//!
//! ```text
//! stross-proto     线协议本职：消息/帧/时间 wire 类型、wire 字符串单一真源
//!   └─ stross-endpoint（本 crate）：端点契约真源 + 分享端/订阅端实现区
//!        └─ stross-kernel：纯管理调度（会话/鉴权/协商/注册表/路由）
//!             └─ stross-bridge / 壳层
//! ```
//!
//! ## 端点分目录（分享端与订阅端独立契约，互不承载对方逻辑）
//!
//! * [`share`]（**分享端点 = 内容源**）：屏幕 / 麦克风 / 系统声音 / 文件(发)，
//!   `load` 探测 + `share` 开推——不实现任何播放/接收逻辑；
//! * [`subscribe`]（**订阅端点 = 内容宿**）：播放器（Graph/Audio 类统一接收
//!   解码）/ 文件(收) 落盘——不实现任何采集/分享逻辑；
//! * [`contract`]：**契约真源**（[`Endpoint`] 公共视图 /
//!   [`ShareEndpoint`] / [`SubscribeEndpoint`] / [`MediaSourceEndpoint`] 分享端
//!   类实现 / 四个能力 trait [`StreamHost`] / [`FileHost`] / [`MediaHost`] /
//!   [`Runtime`]（组合 [`ShareHost`]）内核调度能力）；
//! * [`data`]：端点与内核之间传递的数据契约（[`StreamConfig`] 等）。
//!
//! ## 采集与还原机制（分享端与订阅端的执行工具）
//!
//! * [`capture`]：采集后端抽象 [`CaptureBackend`] + 桌面 [`FfmpegBackend`]
//! * [`pipeline`]：ffmpeg 采集与编码管线（[`StreamConfig`] / [`StreamSession`]）
//! * [`sources`]：摄像头 / 麦克风 / 系统声音端点源枚举
//! * [`playback`]：播放后端（ffmpeg 子进程解码 + cpal 输出）
//! * [`codec`] / [`convert`]：切帧 / 像素转换——源与还原两侧共用
//!
//! 依赖方向：本 crate 只依赖 [`stross_proto`] / [`stross_view`] 与通用库，
//! **不依赖内核**——端点经四个能力 trait（[`StreamHost`] / [`FileHost`] /
//! [`MediaHost`] / [`Runtime`]）契约调用内核调度能力。

pub mod capture;
pub mod codec;
pub mod contract;
pub mod convert;
pub mod data;
pub mod factory;
pub mod pipeline;
pub mod playback;
pub mod share;
pub mod sources;
pub mod subscribe;

pub use capture::{CaptureBackend, CaptureStatus};
pub use codec::adts::AdtsSplitter;
pub use codec::nal::{AccessUnitBuilder, AnnexBSplitter, extract_avc_csd};
pub use contract::{
    Endpoint, EndpointBase, EndpointClass, FileHost, MediaHost, MediaSourceEndpoint, Probe,
    Runtime, ShareEndpoint, ShareHost, StreamHost, SubscribeCtx, SubscribeEndpoint, SubscribeHost,
    TargetKind,
};
pub use convert::rgba::rgba_scaled;
pub use convert::yuv::{Yuv420Layout, yuv420_to_rgba_scaled};
pub use factory::{platform_endpoints, seed_platform_endpoints};
pub use pipeline::{
    AudioSourceConfig, Quality, StreamConfig, StreamSession, VideoSource, ffmpeg_available,
    ffmpeg_bin,
};
#[cfg(not(target_os = "android"))]
pub use playback::FfmpegPlaybackSink;
pub use playback::{
    AudioOut, AudioOutSpec, PlaybackConfig, PlaybackError, PlaybackSession, PlaybackSink,
    PlaybackStats, RenderedFrame, VideoOut,
};
pub use share::{
    FileChannelEndpoint, FileEndpoint, FilePushOptions, MicEndpoint, ScreenEndpoint,
    SystemAudioEndpoint,
};
pub use sources::{CameraEndpoint, list_audio_inputs, list_cameras, list_system_audio};
pub use subscribe::{FileChannelSubscribeEndpoint, FileReceiveEndpoint, MediaReceiveEndpoint};
