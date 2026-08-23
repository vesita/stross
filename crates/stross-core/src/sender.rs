//! 推流引擎：把中继、推流客户端和采集会话组合成开箱即用的推流端。
//!
//! ```text
//! SenderEngine
//! ├── RelayServer（内嵌中继，可选）
//! ├── RelayClient（WS 推流客户端，连到内嵌或外部中继）
//! └── StreamSession（ffmpeg 子进程 + 读管道）
//! ```

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use stross_proto::frame::Frame;
use stross_proto::message::ControlMessage;

use crate::pipeline::{StreamConfig, StreamSession};
use crate::relay::{RelayHandle, RelayServer};

/// 完整的推流引擎。
pub struct SenderEngine {
    relay: Option<RelayHandle>,
    client: RelayClient,
    session: StreamSession,
}

impl SenderEngine {
    /// 启动推流。
    ///
    /// * `relay_url`：`Some("ws://host:port")` 表示推到外部中继；
    ///   `None` 表示启动内嵌中继（绑定 `bind_port`，0 = 自动分配）。
    pub async fn start(
        cfg: StreamConfig,
        relay_url: Option<String>,
        bind_port: u16,
    ) -> Result<Self> {
        let relay = match &relay_url {
            Some(_) => None,
            None => Some(RelayServer::start(bind_port).await?),
        };
        let url = match &relay_url {
            Some(u) => u.clone(),
            None => format!(
                "ws://127.0.0.1:{}/ws/push",
                relay.as_ref().expect("内嵌中继必然存在").port
            ),
        };
        let (client, tx) = RelayClient::connect(&url, &cfg).await?;
        let session = StreamSession::spawn(&cfg, tx)?;
        Ok(Self {
            relay,
            client,
            session,
        })
    }

    /// 内嵌中继端口（未内嵌时为 `None`）。
    pub fn relay_port(&self) -> Option<u16> {
        self.relay.as_ref().map(|r| r.port)
    }

    /// 停止推流：结束采集 → 优雅 Bye → 关闭内嵌中继。
    pub async fn stop(mut self) {
        self.session.stop().await;
        self.client.stop().await;
        if let Some(r) = self.relay.take() {
            r.stop().await;
        }
    }
}

/// WS 推流客户端。
pub struct RelayClient {
    task: JoinHandle<()>,
    connected: watch::Receiver<bool>,
    shutdown: watch::Sender<bool>,
}

impl RelayClient {
    /// 连接中继并发送 `Hello`；返回本客户端与帧通道。
    pub async fn connect(url: &str, cfg: &StreamConfig) -> Result<(Self, mpsc::Sender<Frame>)> {
        let (ws, _resp) = tokio_tungstenite::connect_async(url)
            .await
            .context("连接中继失败")?;
        let (tx, rx) = mpsc::channel::<Frame>(256);
        let (connected_tx, connected_rx) = watch::channel(true);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let hello = ControlMessage::Hello {
            stream_id: cfg.stream_id.clone(),
            title: cfg.title.clone(),
            video: cfg.video_track_info(),
            audio: cfg.audio_track_info(),
        };
        let task = tokio::spawn(client_loop(ws, rx, hello, connected_tx, shutdown_rx));
        Ok((
            Self {
                task,
                connected: connected_rx,
                shutdown: shutdown_tx,
            },
            tx,
        ))
    }

    /// 是否仍连接中。
    pub fn is_connected(&self) -> bool {
        *self.connected.borrow()
    }

    /// 停止客户端：发送 `Bye` 并优雅关闭连接。
    pub async fn stop(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }
}

/// 推流循环：Hello → 转发帧 → 通道关闭或收到停止信号时 Bye。
async fn client_loop<S>(
    mut ws: S,
    mut rx: mpsc::Receiver<Frame>,
    hello: ControlMessage,
    connected: watch::Sender<bool>,
    mut shutdown: watch::Receiver<bool>,
) where
    S: SinkExt<tungstenite::Message, Error = tungstenite::Error> + Unpin,
    S: StreamExt<Item = Result<tungstenite::Message, tungstenite::Error>>,
{
    let _ = ws
        .send(tungstenite::Message::Text(hello.to_text().into()))
        .await;
    loop {
        tokio::select! {
            frame = rx.recv() => {
                match frame {
                    Some(f) => {
                        if ws.send(tungstenite::Message::Binary(f.to_bytes())).await.is_err() {
                            tracing::warn!("推流连接断开");
                            let _ = connected.send(false);
                            break;
                        }
                    }
                    None => {
                        // 会话结束：优雅 Bye
                        let _ = ws
                            .send(tungstenite::Message::Text(ControlMessage::Bye.to_text().into()))
                            .await;
                        let _ = ws.close().await;
                        break;
                    }
                }
            }
            _ = shutdown.changed() => {
                // 主动停止：优雅 Bye
                let _ = ws
                    .send(tungstenite::Message::Text(ControlMessage::Bye.to_text().into()))
                    .await;
                let _ = ws.close().await;
                break;
            }
            incoming = ws.next() => {
                match incoming {
                    Some(Ok(tungstenite::Message::Text(text))) => {
                        if let Ok(ControlMessage::Welcome { .. }) = ControlMessage::from_text(&text) {
                            tracing::info!("中继已确认推流");
                        } else if let Ok(ControlMessage::Error { message }) = ControlMessage::from_text(&text) {
                            tracing::error!("中继错误: {message}");
                            let _ = connected.send(false);
                            break;
                        }
                    }
                    Some(Ok(tungstenite::Message::Close(_))) => {
                        tracing::warn!("推流连接被对端关闭");
                        break;
                    }
                    Some(Err(e)) => {
                        tracing::warn!("推流连接异常: {e}");
                        let _ = connected.send(false);
                        break;
                    }
                    Some(Ok(_)) => {}
                    None => {
                        tracing::warn!("推流连接已断开（无更多消息）");
                        let _ = connected.send(false);
                        break;
                    }
                }
            }
        }
    }
}
