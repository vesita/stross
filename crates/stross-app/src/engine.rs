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

use stross_core::relay::{RelayHandle, RelayServer};
use stross_core::sender::RelayClient;
use stross_media::capture::{CaptureBackend, CaptureStatus};
use stross_media::pipeline::StreamConfig;

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
    /// * `relay_url`：`Some("ws://host:port")` 表示推到外部中继；
    ///   `None` 表示启动内嵌中继（绑定 `bind_port`，0 = 自动分配）。
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
                match RelayServer::start(bind_port).await {
                    Ok(h) => Some(h),
                    Err(_) => {
                        tracing::warn!("端口 {bind_port} 被占用，内嵌中继回退到随机端口");
                        Some(RelayServer::start(0).await?)
                    }
                }
            }
        };
        let url = match &relay_url {
            Some(u) => u.clone(),
            None => format!(
                "ws://127.0.0.1:{}/ws/push",
                relay.as_ref().expect("内嵌中继必然存在").port
            ),
        };
        let (client, tx) = RelayClient::connect(&url, cfg.hello()).await?;
        // 采集启动失败时回滚已建立的推流连接，避免留下半开会话
        if let Err(e) = backend.start(&cfg, tx) {
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

    /// 停止推流：结束采集 → 优雅 Bye → 关闭内嵌中继。
    pub async fn stop(mut self) {
        self.backend.stop();
        self.client.stop().await;
        if let Some(r) = self.relay.take() {
            r.stop().await;
        }
    }
}
