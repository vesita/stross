//! 推流引擎：把共享模块（中继 + 推流客户端）与系统适配模块（采集后端）组合成开箱即用的推流端。
//!
//! ```text
//! SenderEngine
//! ├── RelayServer（内嵌中继，可选）
//! ├── RelayClient（WS 推流客户端，连到内嵌或外部中继）
//! └── CaptureBackend（采集后端：桌面 ffmpeg / Android 原生）
//! ```

use std::sync::Arc;

use anyhow::Result;

use crate::relay::{RelayHandle, RelayServer};
use crate::sender::RelayClient;
use stross_endpoint::capture::{CaptureBackend, CaptureStatus};
use stross_endpoint::pipeline::StreamConfig;

/// 完整的推流引擎。
pub struct SenderEngine {
    relay: Option<RelayHandle>,
    client: RelayClient,
    backend: Arc<dyn CaptureBackend>,
}

impl SenderEngine {
    /// 启动推流。
    ///
    /// * `backend`：平台相关的采集后端（桌面 ffmpeg / Android 原生），以 `Arc` 共享
    /// * `relay_url`：`Some("ws://host:port/ws/push")` / `srt://host:port` /
    ///   `quic://host:port` 表示推到外部中继（按 scheme 选传输）；
    ///   `None` 表示启动内嵌中继（绑定 `bind_port`，0 = 自动分配），
    ///   推流地址按媒体类型自动选传输。
    pub async fn start(
        cfg: StreamConfig,
        backend: Arc<dyn CaptureBackend>,
        relay_url: Option<String>,
        bind_port: u16,
    ) -> Result<Self> {
        let relay = match &relay_url {
            Some(_) => None,
            None => {
                // 优先指定端口；被占用时回退随机端口，保证内嵌中继必然可用
                if let Ok(h) = RelayServer::start(bind_port).await {
                    Some(h)
                } else {
                    tracing::warn!("端口 {bind_port} 被占用，内嵌中继回退到随机端口");
                    Some(RelayServer::start(0).await?)
                }
            }
        };
        let url = if let Some(u) = &relay_url {
            u.clone()
        } else {
            let relay = relay.as_ref().expect("内嵌中继必然存在");
            relay.auto_push_url(cfg.video.is_some())
        };
        let (client, tx) = RelayClient::connect(&url, cfg.hello()).await?;
        // 采集启动失败时回滚已建立的推流连接，避免留下半开会话
        if let Err(e) = backend.start(&cfg, tx).await {
            client.stop().await;
            if let Some(r) = relay {
                r.stop().await;
            }
            return Err(e);
        }
        Ok(Self {
            relay,
            client,
            backend,
        })
    }

    /// 内嵌中继端口（未内嵌时为 `None`）。
    pub fn relay_port(&self) -> Option<u16> {
        self.relay.as_ref().map(|r| r.port)
    }

    /// 采集后端的真实状态（Android 由原生控制帧异步回报）。
    pub fn capture_status(&self) -> CaptureStatus {
        self.backend.status()
    }

    /// 会话起点墙上时刻（Unix 毫秒；延迟校准用 `--report-start`）。
    pub fn wall_start_unix_ms(&self) -> Option<u64> {
        self.backend.wall_start_unix_ms()
    }

    /// 首帧墙时刻（Unix 毫秒；`None` = ffmpeg 尚未输出首帧）。
    pub fn first_frame_wall_unix_ms(&self) -> Option<u64> {
        self.backend.first_frame_wall_unix_ms()
    }

    /// 首帧 pts（毫秒；与首帧墙时刻成对，校准 pts0 修正用）。
    pub fn first_frame_pts_ms(&self) -> Option<u32> {
        self.backend.first_frame_pts_ms()
    }

    /// 停止推流：结束采集 → 优雅 Bye → 关闭内嵌中继。
    pub async fn stop(mut self) {
        self.backend.stop();
        self.client.stop().await;
        if let Some(r) = self.relay.take() {
            r.stop().await;
        }
    }
}
