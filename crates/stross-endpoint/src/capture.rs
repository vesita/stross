//! 采集后端抽象。
//!
//! [`CaptureBackend`] 把"把本机媒体源变成 [`Frame`] 流"这件事抽象成统一接口，
//! 上层（推流引擎 / 应用状态机）只依赖这个 trait，不关心具体平台：
//!
//! * **桌面**：[`FfmpegBackend`] —— ffmpeg 子进程采集（见 [`super::pipeline`]）
//! * **Android**：UI 层用 MediaProjection + MediaCodec 实现（见 `apps/stross-gui` 的 `mobile.rs`）
//!
//! 约定：
//!
//! * `start` 把帧送入调用方给的 `tx`，实现内部必须自行持有 `tx` 的克隆，
//!   保证推流通道在采集会话存续期间不关闭。
//! * `stop` 停止采集并释放持有的 `tx`（通道关闭会触发推流端优雅 Bye）。
//! * `status` 返回采集的真实状态（Android 上由原生控制帧异步回报）。

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use stross_proto::frame::Frame;
use stross_proto::message::{
    CapabilityDescriptor, CapabilityKind, CodecId, MediaKind, ReliabilityProfile, TransportId,
};

use crate::pipeline::{StreamConfig, StreamSession};

/// 采集状态（供 UI 轮询，替代旧的 `mobile_status`）。
#[derive(Debug, Default, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureStatus {
    /// 采集是否真正启动。
    pub started: bool,
    /// 启动失败原因。
    pub error: Option<String>,
}

/// 采集后端：把本机媒体源变成协议帧流。
pub trait CaptureBackend: Send + Sync {
    /// 能力描述（能力广播 / 协商用；默认实现返回未知）。
    ///
    /// 见 docs/plugin-architecture.md §6.1——Source 能力向内核能力注册表上报。
    fn descriptor(&self) -> CapabilityDescriptor {
        CapabilityDescriptor::unknown()
    }
    /// 启动采集，帧送入 `tx`。
    ///
    /// 返回 `Ok` 只代表采集已发起；真实是否就绪由 [`CaptureBackend::status`] 回报
    /// （Android 需要等待系统授权 / 投影就绪）。
    fn start(&self, cfg: &StreamConfig, tx: mpsc::Sender<Frame>) -> anyhow::Result<()>;
    /// 停止采集。
    fn stop(&self);
    /// 当前采集状态。
    fn status(&self) -> CaptureStatus;
    /// 会话起点墙上时刻（Unix 毫秒；延迟校准用，`receive --calibrate` 消费）。
    /// 默认 `None`（未知 / 未启动）。
    fn wall_start_unix_ms(&self) -> Option<u64> {
        None
    }
    /// 首帧墙时刻（Unix 毫秒；`None` = 尚未输出首帧）。延迟校准精确用：
    /// 调用方轮询到 `Some` 再写 `--report-start`（排除 ffmpeg 预热）。
    /// 默认 `None`。
    fn first_frame_wall_unix_ms(&self) -> Option<u64> {
        None
    }
    /// 首帧 pts（毫秒；与首帧墙时刻成对，校准 pts0 修正用）。默认 `None`。
    fn first_frame_pts_ms(&self) -> Option<u32> {
        None
    }
}

/// 桌面端采集后端：ffmpeg 子进程（见 [`crate::pipeline::StreamSession`]）。
///
/// Wayland 屏幕共享：内部路由到 portal+pipewire 采集（见
/// [`crate::screen::wayland`]），启动/运行错误经 `error_rx` 转发到
/// [`CaptureStatus::error`]（桌面侧 `CaptureStatusView` 轮询展示）。
pub struct FfmpegBackend {
    session: Mutex<Option<StreamSession>>,
    status: Arc<Mutex<CaptureStatus>>,
}

impl FfmpegBackend {
    pub fn new() -> Self {
        Self {
            session: Mutex::new(None),
            status: Arc::new(Mutex::new(CaptureStatus::default())),
        }
    }
}

impl Default for FfmpegBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl CaptureBackend for FfmpegBackend {
    fn descriptor(&self) -> CapabilityDescriptor {
        CapabilityDescriptor {
            kind: CapabilityKind::Source,
            media: vec![
                MediaKind::Screen,
                MediaKind::Camera,
                MediaKind::Mic,
                MediaKind::SystemAudio,
            ],
            codecs: vec![CodecId::H264, CodecId::Aac],
            transports: vec![TransportId::Ws],
            max_width: Some(1920),
            max_height: Some(1080),
            preferred_profile: ReliabilityProfile::Lossy,
        }
    }

    fn start(&self, cfg: &StreamConfig, tx: mpsc::Sender<Frame>) -> anyhow::Result<()> {
        let mut session = StreamSession::spawn(cfg, tx)?;
        // Wayland 采集错误（portal 拒绝 / 协商失败）→ CaptureStatus.error；
        // 流会随 ffmpeg stdin 关闭自然结束
        if let Some(mut error_rx) = session.take_error_rx() {
            let status = self.status.clone();
            tokio::spawn(async move {
                while let Some(e) = error_rx.recv().await {
                    tracing::warn!("采集错误: {e}");
                    status.lock().unwrap().error = Some(e);
                }
            });
        }
        let mut status = self.status.lock().unwrap();
        status.started = true;
        status.error = None;
        *self.session.lock().unwrap() = Some(session);
        Ok(())
    }

    fn stop(&self) {
        if let Some(mut session) = self.session.lock().unwrap().take() {
            // ffmpeg 子进程停止是 async 操作（kill + wait），这里只发起，
            // 通道会随会话持有的 tx 释放而关闭
            tokio::spawn(async move {
                session.stop().await;
            });
        }
        *self.status.lock().unwrap() = CaptureStatus::default();
    }

    fn status(&self) -> CaptureStatus {
        self.status.lock().unwrap().clone()
    }

    fn wall_start_unix_ms(&self) -> Option<u64> {
        let session = self.session.lock().unwrap();
        let s = session.as_ref()?;
        // 优先首帧墙时刻（pts=0 对应此时刻，排除 ffmpeg 预热）；未出帧时回退
        // 会话起点（spawn 时刻，含预热上界）
        let wall = s
            .first_frame
            .lock()
            .unwrap()
            .map(|(w, _)| w)
            .unwrap_or(s.started_wall);
        Some(
            wall.duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64),
        )
    }

    /// 首帧墙时刻（Unix 毫秒；`None` = ffmpeg 尚未输出首帧）。
    /// 延迟校准精确用：调用方应轮询到 `Some` 再写 `--report-start`。
    fn first_frame_wall_unix_ms(&self) -> Option<u64> {
        let session = self.session.lock().unwrap();
        let guard = session.as_ref()?.first_frame.lock().unwrap();
        let (wall, _) = guard.as_ref()?;
        Some(
            wall.duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64),
        )
    }

    /// 首帧 pts（毫秒；与 [`Self::first_frame_wall_unix_ms`] 成对，延迟校准
    /// 的 pts0 修正用）。
    fn first_frame_pts_ms(&self) -> Option<u32> {
        let session = self.session.lock().unwrap();
        let guard = session.as_ref()?.first_frame.lock().unwrap();
        Some(guard.as_ref()?.1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmpeg_backend_idle_status() {
        let backend = FfmpegBackend::new();
        let status = backend.status();
        assert!(!status.started);
        assert!(status.error.is_none());
    }
}
