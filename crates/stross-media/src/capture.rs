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

use std::sync::Mutex;

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
}

/// 桌面端采集后端：ffmpeg 子进程（见 [`crate::pipeline::StreamSession`]）。
pub struct FfmpegBackend {
    session: Mutex<Option<StreamSession>>,
    status: Mutex<CaptureStatus>,
}

impl FfmpegBackend {
    pub fn new() -> Self {
        Self {
            session: Mutex::new(None),
            status: Mutex::new(CaptureStatus::default()),
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
        let session = StreamSession::spawn(cfg, tx)?;
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
