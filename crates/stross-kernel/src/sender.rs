//! 推流客户端：连接中继（内嵌或外部），发送 `Hello` 后持续转发媒体帧。
//!
//! ```text
//! RelayClient
//! └── client_loop（Hello → 转发帧 → 通道关闭或停止信号时 Bye）
//! ```
//!
//! 基于传输抽象（[`Transport`](crate::transport::Transport)）拨号：
//! `ws://`（无损）/ `srt://`（自适应，弱网/跨 NAT）/ `quic://`（无损，多路复用）。
//! 连接后等待 relay 回 `Welcome` 才返回，保证推流端注册完成（观看端不会竞态）。

use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use stross_proto::frame::Frame;
use stross_proto::message::ControlMessage;

use crate::transport::{DataSession, PeerAddr, SessionPacket, SessionParams};

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
    /// `url` 支持三种传输：`ws://host/ws/push`（无损）、
    /// `srt://host:port`（自适应，relay 的 [`RelayHandle::srt_port`]）与
    /// `quic://host:port`（无损多路复用，relay 的 [`RelayHandle::quic_port`]）。
    pub async fn connect(url: &str, hello: ControlMessage) -> Result<(Self, mpsc::Sender<Frame>)> {
        // 地址解析收口在 RelayUrl：未知 scheme / 缺端口提前失败（错误信息优于
        // 落到传输层再报「连接失败」）
        crate::transport::RelayUrl::parse(url)
            .ok_or_else(|| anyhow::anyhow!("无法解析中继地址: {url}"))?;
        let stream_id = match &hello {
            ControlMessage::Hello { stream_id, .. } => stream_id.clone(),
            _ => stross_view::id::StreamId::default(),
        };
        let transport = crate::transport::transport_for_url(url);
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
        let (welcome_tx, mut welcome_rx) = watch::channel(false);

        let task = tokio::spawn(client_loop(
            session,
            rx,
            hello,
            connected_tx,
            shutdown_rx,
            welcome_tx,
        ));
        // 等 relay 回 Welcome：`start_stream` 返回时流已注册、可被观看端发现。
        // （SRT/QUIC 握手慢于 WS watch，不等待会竞态：观看端先到报「流不存在」）
        tokio::time::timeout(Duration::from_secs(5), async {
            while !*welcome_rx.borrow() {
                if welcome_rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("等待中继 Welcome 超时（中继可能未回确认）"))?;
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
    welcome: watch::Sender<bool>,
) {
    let _ = session.send(SessionPacket::Control(hello)).await;
    // 会话内单调递增帧序号（B5）：有损路径（SRT）的接收端抖动缓冲按 seq
    // 排序/判空洞；无损路径（WS/QUIC）接收端直通，seq 无副作用。
    let mut next_seq: u32 = 0;
    loop {
        tokio::select! {
            frame = rx.recv() => {
                if let Some(mut f) = frame {
                    f.header.seq = next_seq;
                    next_seq = next_seq.wrapping_add(1);
                    if session.send(SessionPacket::Media(f)).await.is_err() {
                        tracing::warn!("推流连接断开");
                        let _ = connected.send(false);
                        break;
                    }
                } else {
                    // 会话结束：优雅 Bye
                    let _ = session
                        .send(SessionPacket::Control(ControlMessage::Bye))
                        .await;
                    let _ = session.close().await;
                    break;
                }
            }
            _ = shutdown.changed() => {
                // 主动停止：先把帧通道里已排队的帧发完再优雅 Bye——文件传输
                // （TRACK_FILE）依赖该 Graceful 语义：末块在 stop 前已入队，
                // 若直接 Bye 会被 select 抢跑丢弃（实测空/小文件丢末帧）。
                // 媒体路径无副作用：后端已停发，队列里至多是最后几帧。
                while let Ok(mut f) = rx.try_recv() {
                    f.header.seq = next_seq;
                    next_seq = next_seq.wrapping_add(1);
                    if session.send(SessionPacket::Media(f)).await.is_err() {
                        break;
                    }
                }
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
                        let _ = welcome.send(true);
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
