//! WebSocket 传输实现。
//!
//! * 客户端拨号：[`WsTransport::connect`]（tokio-tungstenite，推到中继）
//! * 服务端接入：[`WsTransport::from_socket`]（包装外部已升级的 socket）
//!
//! 传输层不依赖任何 HTTP 框架：服务端 socket 由调用方（HTTP 层，如
//! `stross-core::relay::http`）把已升级的连接适配成 [`WsIo`] 传入，
//! axum 类型只存在于调用方（docs/layering-architecture.md：core 拥有
//! 中继 HTTP API，transport 只描述传输）。
//!
//! 控制消息走文本帧（JSON），媒体帧走二进制帧（帧头 + 载荷），与现有
//! `/ws/push`、`/ws/watch` 的线上格式完全一致。

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::net::TcpStream;
use tokio::sync::Mutex;

use stross_proto::message::{ReliabilityProfile, TransportId};

use super::{
    DataSession, PeerAddr, SessionPacket, SessionParams, SharedStats, Transport, TransportError,
    TransportStats,
};

/// WebSocket 传输（无损）。
pub struct WsTransport {
    stats: SharedStats,
}

impl Default for WsTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl WsTransport {
    pub fn new() -> Self {
        Self {
            stats: Arc::new(TransportStats::default()),
        }
    }

    /// 服务端：把一个**已升级的 socket 适配**包装成数据会话。
    ///
    /// `io` 由 HTTP 层（axum 等具体框架）把已升级的连接适配成 [`WsIo`] 传入，
    /// 传输层不感知具体框架。`peer` 为对端地址（HTTP 升级处经 `ConnectInfo`
    /// 提取）；来源感知门控（回环 = 本机预授权，非回环 = 凭证接入）依赖它，
    /// 未知来源按非回环对待。
    pub fn from_socket(
        &self,
        io: Box<dyn WsIo>,
        peer: Option<std::net::SocketAddr>,
    ) -> Box<dyn DataSession> {
        Box::new(WsDataSession {
            io,
            stats: self.stats.clone(),
            peer,
        })
    }
}

#[async_trait]
impl Transport for WsTransport {
    fn id(&self) -> TransportId {
        TransportId::Ws
    }

    fn profile(&self) -> ReliabilityProfile {
        ReliabilityProfile::Lossless
    }

    async fn connect(
        &self,
        peer: &PeerAddr,
        _params: &SessionParams,
    ) -> Result<Box<dyn DataSession>, TransportError> {
        let (socket, _resp) = tokio_tungstenite::connect_async(&peer.addr)
            .await
            .map_err(|e| TransportError::Connect(e.to_string()))?;
        // TCP_NODELAY：媒体每帧一个 WS 消息，Nagle 会叠加延迟
        if let tokio_tungstenite::MaybeTlsStream::Plain(tcp) = socket.get_ref() {
            let _ = tcp.set_nodelay(true);
        }
        Ok(Box::new(WsDataSession {
            io: Box::new(TungsteniteWs::new(socket)),
            stats: self.stats.clone(),
            peer: peer.addr.parse().ok(),
        }))
    }

    async fn accept(
        &self,
        _params: &SessionParams,
    ) -> Result<Box<dyn DataSession>, TransportError> {
        Err(TransportError::NotSupported(
            "ws 服务端请使用 WsTransport::from_socket（HTTP 升级处构造 WsIo）",
        ))
    }

    fn stats(&self) -> TransportStats {
        self.stats.as_ref().clone()
    }
}

/// WS 线上消息（客户端/服务端 socket 的统一视图，供 [`WsIo`] 适配实现使用）。
pub enum WsMsg {
    Text(String),
    Binary(Bytes),
}

/// 底层 WS 通道适配（客户端 tungstenite 内置实现；服务端由 HTTP 层实现，
/// 如 `stross-core::relay::http::AxumWs`——传输层不依赖具体 HTTP 框架）。
#[async_trait]
pub trait WsIo: Send + Sync + 'static {
    async fn send_msg(&self, msg: WsMsg) -> Result<(), TransportError>;
    /// `Ok(None)` 表示连接已关闭（Close 帧或 EOF）。
    async fn recv_msg(&self) -> Result<Option<WsMsg>, TransportError>;
    async fn close(&self) -> Result<(), TransportError>;
}

type WsSink = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
    tokio_tungstenite::tungstenite::Message,
>;
type WsStream = futures_util::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
>;

/// tungstenite 客户端 socket 适配。
///
/// 读写分离架构（SplitSink + SplitStream）：
/// - `send_msg` 仅锁写半部（sink），`recv_msg` 仅锁读半部（stream），避免双工场景下收发互锁与队头阻塞；
/// - 心跳保活：读到 Ping 自动响应 Pong，非阻塞且保持链路存活；
/// - 优雅断开：close 时优雅发送 Close 帧并释放资源。
struct TungsteniteWs {
    sink: Mutex<Option<WsSink>>,
    stream: Mutex<Option<WsStream>>,
}

impl TungsteniteWs {
    fn new(
        socket: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
    ) -> Self {
        use futures_util::StreamExt;
        let (sink, stream) = socket.split();
        Self {
            sink: Mutex::new(Some(sink)),
            stream: Mutex::new(Some(stream)),
        }
    }
}

#[async_trait]
impl WsIo for TungsteniteWs {
    async fn send_msg(&self, msg: WsMsg) -> Result<(), TransportError> {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::Message as M;
        let msg = match msg {
            WsMsg::Text(s) => M::Text(s.into()),
            WsMsg::Binary(b) => M::Binary(b),
        };
        let mut sink_guard = self.sink.lock().await;
        let sink = sink_guard.as_mut().ok_or(TransportError::Closed)?;
        sink.send(msg)
            .await
            .map_err(|e| TransportError::Io(format!("WebSocket 发送失败: {e}")))
    }

    async fn recv_msg(&self) -> Result<Option<WsMsg>, TransportError> {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message as M;
        loop {
            let mut stream_guard = self.stream.lock().await;
            let stream = stream_guard.as_mut().ok_or(TransportError::Closed)?;
            let item = stream.next().await;
            drop(stream_guard);

            match item {
                Some(Ok(M::Text(t))) => return Ok(Some(WsMsg::Text(t.to_string()))),
                Some(Ok(M::Binary(b))) => return Ok(Some(WsMsg::Binary(b))),
                Some(Ok(M::Close(_))) | None => return Ok(None),
                Some(Ok(M::Ping(p))) => {
                    // 收到 Ping 帧，立即非阻塞回复 Pong 维持连接存活
                    let mut sink_guard = self.sink.lock().await;
                    if let Some(sink) = sink_guard.as_mut() {
                        let _ = sink.send(M::Pong(p)).await;
                    }
                    continue;
                }
                Some(Ok(M::Pong(_))) => {
                    // 收到 Pong 心跳应答，继续接收数据
                    continue;
                }
                Some(Ok(M::Frame(_))) => continue,
                Some(Err(e)) => {
                    let err_str = e.to_string();
                    // 针对连接重置、Broken pipe 等典型对端断开错误，按正常 EOF 关闭处理以支持上层重连
                    if err_str.contains("Connection reset")
                        || err_str.contains("Broken pipe")
                        || err_str.contains("closed")
                    {
                        return Ok(None);
                    }
                    return Err(TransportError::Io(format!("WebSocket 接收失败: {err_str}")));
                }
            }
        }
    }

    async fn close(&self) -> Result<(), TransportError> {
        use futures_util::SinkExt;
        let mut sink_guard = self.sink.lock().await;
        if let Some(mut sink) = sink_guard.take() {
            let _ = sink
                .send(tokio_tungstenite::tungstenite::Message::Close(None))
                .await;
            let _ = sink.close().await;
        }
        let mut stream_guard = self.stream.lock().await;
        stream_guard.take();
        Ok(())
    }
}

/// WS 数据会话：把 [`SessionPacket`] 映射为文本/二进制帧。
struct WsDataSession {
    io: Box<dyn WsIo>,
    stats: SharedStats,
    /// 对端地址（来源感知门控；服务端来自 HTTP 升级处，客户端来自拨号地址）。
    peer: Option<std::net::SocketAddr>,
}

#[async_trait]
impl DataSession for WsDataSession {
    fn peer_addr(&self) -> Option<std::net::SocketAddr> {
        self.peer
    }

    async fn send(&self, pkt: SessionPacket) -> Result<(), TransportError> {
        let (msg, size) = match pkt {
            SessionPacket::Control(c) => {
                let s = c.to_text();
                let n = s.len();
                (WsMsg::Text(s), n)
            }
            SessionPacket::Media(f) => {
                let b = f.to_bytes();
                let n = b.len();
                (WsMsg::Binary(b), n)
            }
        };
        self.io.send_msg(msg).await?;
        self.stats.add_sent(size);
        Ok(())
    }

    async fn recv(&self) -> Result<Option<SessionPacket>, TransportError> {
        let Some(msg) = self.io.recv_msg().await? else {
            return Ok(None);
        };
        match msg {
            WsMsg::Text(s) => {
                let n = s.len();
                let pkt = stross_proto::message::ControlMessage::from_text(&s)
                    .map(SessionPacket::Control)
                    .map_err(|e| TransportError::Protocol(e.to_string()))?;
                self.stats.add_recv(n);
                Ok(Some(pkt))
            }
            WsMsg::Binary(b) => {
                let n = b.len();
                let frame = stross_proto::frame::Frame::from_bytes_owned(b)
                    .map_err(|e| TransportError::Protocol(e.to_string()))?;
                self.stats.add_recv(n);
                Ok(Some(SessionPacket::Media(frame)))
            }
        }
    }

    async fn close(&self) -> Result<(), TransportError> {
        self.io.close().await
    }
}
