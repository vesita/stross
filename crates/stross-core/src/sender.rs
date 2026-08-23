//! WS 推流客户端：连接中继（内嵌或外部），发送 `Hello` 后持续转发媒体帧。
//!
//! ```text
//! RelayClient
//! └── client_loop（Hello → 转发帧 → 通道关闭或停止信号时 Bye）
//! ```

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use stross_proto::frame::Frame;
use stross_proto::message::ControlMessage;

/// WS 推流客户端。
pub struct RelayClient {
    task: JoinHandle<()>,
    connected: watch::Receiver<bool>,
    shutdown: watch::Sender<bool>,
}

impl RelayClient {
    /// 连接中继并发送 `Hello`；返回本客户端与帧通道。
    ///
    /// `hello` 由调用方构造（如 `StreamConfig::hello()`），
    /// 这样本模块不需要依赖任何采集配置类型。
    pub async fn connect(url: &str, hello: ControlMessage) -> Result<(Self, mpsc::Sender<Frame>)> {
        let (ws, _resp) = tokio_tungstenite::connect_async(url)
            .await
            .context("连接中继失败")?;
        let (tx, rx) = mpsc::channel::<Frame>(256);
        let (connected_tx, connected_rx) = watch::channel(true);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

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
