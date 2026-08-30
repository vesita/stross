//! 屏幕端点（实时目标）：load 探测屏幕采集可用性；share 采集本机屏幕推流。
//!
//! 平台分发（`cfg(target_os)` 只允许出现在这里，与 bridge 端点装配同源）：
//!
//! * **Linux**：[`linux`] —— Wayland 会话走 portal+pipewire（[`wayland`]，
//!   合成器无关的 CPU/SHM 路径）；X11 走 ffmpeg x11grab
//! * **Windows**：[`windows`] —— ffmpeg gdigrab
//! * **macOS**：[`macos`] —— 采集暂不支持（probe 直接不可挂载，原因前置）
//!
//! `share` 一律走 [`VideoSource::Screen`]，采集后端（[`crate::capture`]）内部
//! 按会话类型路由（Wayland → portal+pipewire；否则 ffmpeg 输入源），
//! 上层无感。

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(all(target_os = "linux", feature = "wayland-capture"))]
pub mod wayland;
#[cfg(target_os = "windows")]
pub mod windows;

use std::result::Result as StdResult;
use std::sync::Arc;

use stross_proto::message::{EndpointStrategy, MediaKind, ReliabilityProfile, SerializeRule};

use crate::contract::{
    Endpoint, EndpointApp, EndpointBase, Probe, SubscribeCtx, TargetKind, spawn_media_share,
};
use crate::pipeline::VideoSource;
use stross_proto::message::PickRule;

/// 屏幕端点（实时目标）：load 探测采集可用性（probe 由平台层注入——
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
                id: "screen:0".into(),
                kind: MediaKind::Screen,
                name: name.into(),
                available: false,
                last_error: None,
            },
            probe,
        }
    }
}

impl Endpoint for ScreenEndpoint {
    fn id(&self) -> &str {
        &self.base.id
    }
    fn kind(&self) -> MediaKind {
        self.base.kind
    }
    fn name(&self) -> &str {
        &self.base.name
    }
    fn target(&self) -> TargetKind {
        TargetKind::Live
    }
    fn transport_profile(&self) -> ReliabilityProfile {
        // 屏幕实时视频：允许丢帧（关键帧对齐自愈），低延迟
        ReliabilityProfile::Lossy
    }
    fn strategy(&self) -> EndpointStrategy {
        // 实时目标：直通序列化 + 严格即时（Realtime）
        EndpointStrategy {
            strategy_id: EndpointStrategy::DEFAULT_ID.into(),
            serialize: SerializeRule::Passthrough,
            pick: PickRule::Realtime,
        }
    }
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
    fn share(&self, app: Arc<dyn EndpointApp>, ctx: SubscribeCtx) {
        spawn_media_share(
            app,
            ctx,
            self.id(),
            self.name().to_string(),
            Some(VideoSource::Screen),
            None,
        );
    }
}
