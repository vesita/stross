//! **数据契约**：分享端与订阅端之间传输的纯数据载荷（跨概念共享）。
//!
//! v3 概念（docs/framework-v3.md §3.2 附）：`StreamConfig` / `VideoSource` /
//! `AudioSourceConfig` / `Quality` / `FilePushOptions` 是端点与内核之间传递的
//! 数据契约；序列化/pick 的策略组合（[`EndpointStrategy`]）在 stross-proto。

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use stross_proto::message::{CodecId, ControlMessage, EndpointId, StreamId, TrackInfo};

use crate::contract::{Runtime, StreamHost, SubscribeCtx};

/// 画质档位。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Quality {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

impl Quality {
    pub const LOW: Self = Self {
        width: 640,
        height: 360,
        fps: 24,
        bitrate_kbps: 800,
    };
    pub const MEDIUM: Self = Self {
        width: 1280,
        height: 720,
        fps: 30,
        bitrate_kbps: 2500,
    };
    pub const HIGH: Self = Self {
        width: 1920,
        height: 1080,
        fps: 30,
        bitrate_kbps: 6000,
    };

    /// 预设列表 `(显示名, 配置)`。
    pub const fn presets() -> [(&'static str, Self); 3] {
        [
            ("低 (640×360@24)", Self::LOW),
            ("中 (1280×720@30)", Self::MEDIUM),
            ("高 (1920×1080@30)", Self::HIGH),
        ]
    }

    /// GOP（关键帧间隔，帧数），默认 2 秒。
    pub fn gop(&self) -> u32 {
        (self.fps * 2).max(1)
    }
}

impl Default for Quality {
    fn default() -> Self {
        Self::HIGH
    }
}

/// 视频源。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum VideoSource {
    /// 整个主屏幕（Windows: gdigrab；Linux: x11grab）。
    Screen,
    /// 摄像头；`device` 为 `CameraDevice.id`。
    Camera { device: Option<String> },
    /// lavfi 测试画面（如 `testsrc2`、`smptebars`），方便无设备时演示。
    Synthetic { pattern: String },
}

/// 音频源配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AudioSourceConfig {
    /// 麦克风设备；`None` = 系统默认输入。
    pub mic: Option<String>,
    /// 系统声音（回环采集设备）；`None` = 不采集。
    pub system_audio: Option<String>,
    /// 合成音源（lavfi `sine`，频率 Hz）；`Some` 时取代真实采集，
    /// 无设备环境测试 / 演示用（见播放侧解码回路的集成测试）。
    #[serde(default)]
    pub synthetic: Option<u32>,
    #[serde(default = "default_sample_rate")]
    pub sample_rate: u32,
    #[serde(default = "default_channels")]
    pub channels: u8,
    #[serde(default = "default_audio_bitrate")]
    pub bitrate_kbps: u32,
}

const fn default_sample_rate() -> u32 {
    48_000
}
const fn default_channels() -> u8 {
    2
}
const fn default_audio_bitrate() -> u32 {
    128
}

impl Default for AudioSourceConfig {
    fn default() -> Self {
        Self {
            mic: None,
            system_audio: None,
            synthetic: None,
            sample_rate: default_sample_rate(),
            channels: default_channels(),
            bitrate_kbps: default_audio_bitrate(),
        }
    }
}

impl AudioSourceConfig {
    /// 合成测试音（440Hz sine）：无设备环境下验证音频链路。
    ///
    /// `--audio` 类 CLI 参数用它——此前直接用 [`AudioSourceConfig::default`]
    /// 导致 synthetic/mic/system_audio 全为 `None`，ffmpeg 无音频输入，
    /// 推流实际无声（音频链路从未被真实数据验证，D3 反向音频验收的前提）。
    pub fn synthetic_test() -> Self {
        Self {
            synthetic: Some(440),
            ..Self::default()
        }
    }
}

/// 一次推流的完整配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamConfig {
    pub stream_id: StreamId,
    pub title: String,
    #[serde(default)]
    pub video: Option<VideoSource>,
    #[serde(default)]
    pub quality: Quality,
    #[serde(default)]
    pub audio: Option<AudioSourceConfig>,
    /// 限制推流时长（秒）；`None` = 无限。测试/演示用。
    #[serde(default)]
    pub duration_secs: Option<u32>,
    /// 一次性接入凭证（跨设备推流到对方受控中继用；本机推流为 `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share_token: Option<String>,
}

impl StreamConfig {
    /// CLI 合成源推流配置（测试 / 演示：testsrc2 画面 + 可选 440Hz 测试音）。
    ///
    /// `push` / `ctrl start-stream` / `demo_push` 共用，避免各处手拼字段
    /// （重复实现，曾出现 `--audio` 无声等不一致）。
    pub fn cli_synthetic(
        stream_id: impl Into<StreamId>,
        title: String,
        quality: Quality,
        secs: u32,
        audio: bool,
        share_token: Option<String>,
    ) -> Self {
        let mut cfg = Self {
            stream_id: stream_id.into(),
            title,
            video: Some(VideoSource::Synthetic {
                pattern: "testsrc2".into(),
            }),
            quality,
            audio: None,
            duration_secs: Some(secs),
            share_token,
        };
        if audio {
            cfg.audio = Some(AudioSourceConfig::synthetic_test());
        }
        cfg
    }

    /// 生成推流端注册用的 `Hello` 控制消息。
    pub fn hello(&self) -> ControlMessage {
        ControlMessage::Hello {
            stream_id: self.stream_id.clone(),
            title: self.title.clone(),
            video: self.video_track_info(),
            audio: self.audio_track_info(),
            share_token: self.share_token.clone(),
        }
    }

    /// 生成 Hello 消息里的轨道信息（供观看端展示）。
    pub fn video_track_info(&self) -> Option<TrackInfo> {
        self.video.as_ref().map(|_| TrackInfo {
            codec: CodecId::H264,
            width: Some(self.quality.width),
            height: Some(self.quality.height),
            fps: Some(self.quality.fps),
            sample_rate: None,
            channels: None,
        })
    }

    pub fn audio_track_info(&self) -> Option<TrackInfo> {
        self.audio.as_ref().map(|a| TrackInfo {
            codec: CodecId::Aac,
            width: None,
            height: None,
            fps: None,
            sample_rate: Some(a.sample_rate),
            channels: Some(a.channels),
        })
    }
}

/// 文件泵参数（公开方驱动构造；内核 `push_file` 消费——契约单一真源）。
#[derive(Debug, Clone)]
pub struct FilePushOptions {
    /// 中继推流地址（`ws://host:port/ws/push`；文件走无损 WS 路径）。
    pub push_url: String,
    /// 数据面流 id（pull = 公开方本机会话；push = 订阅方自签会话）。
    pub stream_id: StreamId,
    /// 推流标题（Hello.title；展示用）。
    pub title: String,
    /// 跨设备接入凭证（push 模式 = 订阅方自签；本机 pull = `None`）。
    pub share_token: Option<String>,
    /// 观看数轮询基址（`ws://host:port`；`None` = 不等观看者直接推）。
    pub watcher_base: Option<String>,
}

/// 媒体端点自动推流（实时目标共用）：pull 推本机中继（地址自动），
/// push 凭订阅方凭证出站推入订阅方中继。
///
/// `host`：分享端可见的 [`StreamHost`] 能力（`start_stream` / `relay_port`；
/// 媒体端点只用 StreamHost 部分，见 [`crate::contract::ShareHost`]）。
/// 经 [`Runtime::spawn_task`] 在运行时上下文执行（契约层零 tokio 依赖，
/// 运行时由内核注入）。生命周期治理（watchers=0 自动收尾）已从契约删除，
/// 归未来 `stross-share::ShareService`（docs/framework-v3.md §3.3）。
pub fn spawn_media_share(
    host: &Arc<dyn StreamHost>,
    runtime: &Arc<dyn Runtime>,
    ctx: SubscribeCtx,
    endpoint_id: EndpointId,
    title: String,
    video: Option<VideoSource>,
    audio: Option<AudioSourceConfig>,
) {
    let host2 = host.clone();
    runtime.spawn_task(Box::pin(async move {
        let cfg = StreamConfig {
            stream_id: ctx.stream_id.clone(),
            title,
            video,
            quality: Quality::MEDIUM,
            audio,
            duration_secs: None,
            // 订阅驱动定稿（docs/framework-v3.md §3.4）：只走 pull——推本机
            // 中继，无出站凭证。
            share_token: None,
        };
        let relay_url = resolve_media_url(&ctx);
        match host2.start_stream(cfg, relay_url).await {
            Ok(r) => {
                tracing::info!(
                    "端点 {endpoint_id} 已自动推流: stream={} 订阅方 {}",
                    r.stream_id,
                    ctx.subscriber
                );
            }
            Err(e) => tracing::warn!(
                "端点 {endpoint_id} 自动推流失败（订阅方 {}）: {e:#}",
                ctx.subscriber
            ),
        }
    }));
}

/// 媒体推流的目标地址：订阅驱动定稿只走 pull → `None`（推本机中继，地址由
/// 内核自动选择；无 push 出站路径）。
pub fn resolve_media_url(_ctx: &SubscribeCtx) -> Option<String> {
    None
}

/// 文件泵推送地址：订阅驱动定稿只走 pull → 自己的受控中继（回环地址）。
pub fn resolve_file_url(host: &dyn StreamHost, _ctx: &SubscribeCtx) -> Option<String> {
    let port = host.relay_port()?;
    Some(format!("ws://127.0.0.1:{port}/ws/push"))
}

/// 观看数轮询基址（文件泵等观看者接入用）：订阅驱动定稿只走 pull → 自己中继。
pub fn resolve_watcher_base(host: &dyn StreamHost, _ctx: &SubscribeCtx) -> Option<String> {
    host.relay_port().map(|p| format!("ws://127.0.0.1:{p}"))
}
