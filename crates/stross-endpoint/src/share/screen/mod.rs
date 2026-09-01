//! 屏幕端点（Graph 能力族，docs/endpoint-model-v2.md §3）：load 探测采集
//! 可用性（probe 由平台层注入——无图形会话 / 后端缺失时标记不可挂载，
//! 屏幕获取失败前置化）；分享端走 [`MediaSourceEndpoint`] 统一实现
//! （纯视频源组流推流）。订阅端（播放器）是独立契约
//! [`SubscribeEndpoint`]，见 [`crate::subscribe::media::MediaReceiveEndpoint`]。
//!
//! 平台分发（`cfg(target_os)` 只允许出现在这里，与 bridge 端点装配同源）：
//!
//! * **Linux**：[`linux`] —— Wayland 会话走 portal+pipewire（[`wayland`]，
//!   合成器无关的 CPU/SHM 路径）；X11 走 ffmpeg x11grab
//! * **Windows**：[`windows`] —— ffmpeg gdigrab
//! * **macOS**：[`macos`] —— 采集暂不支持（probe 直接不可挂载，原因前置）
//! * **Android**：[`android`] —— MediaProjection 路径（采集执行在壳层注入的
//!   采集后端；运行时授权异步回报，探测恒可用）
//!
//! `share` 一律走 [`VideoSource::Screen`]，采集后端（[`crate::capture`]）内部
//! 按会话类型路由（Wayland → portal+pipewire；Android → MediaProjection；
//! 否则 ffmpeg 输入源），上层无感。

#[cfg(target_os = "android")]
pub mod android;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(all(target_os = "linux", feature = "wayland-capture"))]
pub mod wayland;
#[cfg(target_os = "windows")]
pub mod windows;

use std::result::Result as StdResult;

use stross_proto::message::{EndpointId, MediaKind};

use crate::contract::{EndpointBase, MediaSourceEndpoint, Probe};
use crate::impl_media_source_endpoint;
use crate::pipeline::{AudioSourceConfig, VideoSource};

/// 屏幕分享端点（Graph 能力族）：load 探测采集可用性（probe 由平台层注入——
/// 无图形会话 / 后端缺失时标记不可挂载，屏幕获取失败前置化）。
pub struct ScreenEndpoint {
    base: EndpointBase,
    probe: Probe,
}

impl ScreenEndpoint {
    /// `probe`：平台适应层注入的屏幕采集可用性探测。
    pub fn new(name: impl Into<String>, probe: Probe) -> Self {
        Self {
            base: EndpointBase {
                id: EndpointId::new(MediaKind::Screen, 0),
                kind: MediaKind::Screen,
                name: name.into(),
                available: false,
                last_error: None,
            },
            probe,
        }
    }
}

impl MediaSourceEndpoint for ScreenEndpoint {
    fn video(&self) -> Option<VideoSource> {
        Some(VideoSource::Screen)
    }
    fn audio(&self) -> Option<AudioSourceConfig> {
        None
    }
}

impl_media_source_endpoint!(ScreenEndpoint {

        fn id(&self) -> EndpointId {
            self.base.id
        }
        fn kind(&self) -> MediaKind {
            self.base.kind
        }
        fn name(&self) -> &str {
            &self.base.name
        }
}, {

        fn available(&self) -> bool {
            self.base.available
        }
        fn last_error(&self) -> Option<&str> {
            self.base.last_error.as_deref()
        }
        fn load(&mut self) -> StdResult<(), String> {
            match (self.probe)() {
                Ok(()) => {
                    self.base.available = true;
                    self.base.last_error = None;
                    Ok(())
                }
                Err(e) => {
                    self.base.mark_failed(e.clone());
                    Err(e)
                }
            }
        }
});
