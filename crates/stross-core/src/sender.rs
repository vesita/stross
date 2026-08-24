//! 推流客户端：连接中继（内嵌或外部），发送 `Hello` 后持续转发媒体帧。
//!
//! ```text
//! RelayClient
//! └── client_loop（Hello → 转发帧 → 通道关闭或停止信号时 Bye）
//! ```
//!
//! 基于传输抽象（[`Transport`](crate::transport::Transport)）拨号：
//! `ws://`（无损，现状）/ `srt://`（自适应，弱网/跨 NAT）/ `quic://`（无损，多路复用）。

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use stross_proto::frame::Frame;
use stross_proto::message::ControlMessage;

use crate::transport::quic::QuicTransport;
use crate::transport::srt::SrtTransport;
use crate::transport::ws::WsTransport;
use crate::transport::{DataSession, PeerAddr, SessionPacket, SessionParams, Transport};

/// 推流客户端。
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
    ///
    /// `url` 支持三种传输：`ws://host/ws/push`（无损，现状）、
    /// `srt://host:port`（自适应，relay 的 [`RelayHandle::srt_port`]）与
    /// `quic://host:port`（无损多路复用，relay 的 [`RelayHandle::quic_port`]）。
    pub async fn connect(url: &str, hello: ControlMessage) -> Result<(Self, mpsc::Sender<Frame>)> {
        let stream_id = match &hello {
            ControlMessage::Hello { stream_id, .. } => stream_id.clone(),
            _ => String::new(),
        };
        let transport: Box<dyn Transport> = if url.starts_with("srt://") {
            Box::new(SrtTransport::new())
        } else if url.starts_with("quic://") {
            Box::new(QuicTransport::new())
        } else {
            Box::new(WsTransport::new())
        };
        let peer = PeerAddr {
            transport: transport.id(),
            addr: url.to_string(),
        };
        let params = SessionParams {
            session_id: stream_id,
            profile: transport.profile(),
        };
        let session = transport
            .connect(&peer, &params)
            .await
            .context("连接中继失败")?;
        let (tx, rx) = mpsc::channel::<Frame>(256);
        let (connected_tx, connected_rx) = watch::channel(true);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let task = tokio::spawn(client_loop(session, rx, hello, connected_tx, shutdown_rx));
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
async fn client_loop(
    session: Box<dyn DataSession>,
    mut rx: mpsc::Receiver<Frame>,
    hello: ControlMessage,
    connected: watch::Sender<bool>,
    mut shutdown: watch::Receiver<bool>,
) {
    let _ = session.send(SessionPacket::Control(hello)).await;
    loop {
        tokio::select! {
            frame = rx.recv() => {
                match frame {
                    Some(f) => {
                        if session.send(SessionPacket::Media(f)).await.is_err() {
                            tracing::warn!("推流连接断开");
                            let _ = connected.send(false);
                            break;
                        }
                    }
                    None => {
                        // 会话结束：优雅 Bye
                        let _ = session
                            .send(SessionPacket::Control(ControlMessage::Bye))
                            .await;
                        let _ = session.close().await;
                        break;
                    }
                }
            }
            _ = shutdown.changed() => {
                // 主动停止：优雅 Bye
                let _ = session
                    .send(SessionPacket::Control(ControlMessage::Bye))
                    .await;
                let _ = session.close().await;
                break;
            }
            incoming = session.recv() => {
                match incoming {
                    Ok(Some(SessionPacket::Control(ControlMessage::Welcome { .. }))) => {
                        tracing::info!("中继已确认推流");
                    }
                    Ok(Some(SessionPacket::Control(ControlMessage::Error { message }))) => {
                        tracing::error!("中继错误: {message}");
                        let _ = connected.send(false);
                        break;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        tracing::warn!("推流连接已断开（无更多消息）");
                        let _ = connected.send(false);
                        break;
                    }
                    Err(e) => {
                        tracing::warn!("推流连接异常: {e}");
                        let _ = connected.send(false);
                        break;
                    }
                }
            }
        }
    }
}
