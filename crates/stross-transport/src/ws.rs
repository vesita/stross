//! WebSocket 传输实现。
//!
//! * 客户端拨号：[`WsTransport::connect`]（tokio-tungstenite，推到中继）
//! * 服务端接入：[`WsTransport::from_upgraded`]（包装已升级的 axum WebSocket）
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

    /// 服务端：把一个已升级的 axum WebSocket 包装成数据会话。
    ///
    /// `peer` 为对端地址（HTTP 升级处经 `ConnectInfo` 提取）；来源感知门控
    /// （回环 = 本机预授权，非回环 = 凭证接入）依赖它，未知来源按非回环对待。
    pub fn from_upgraded(
        &self,
        socket: axum::extract::ws::WebSocket,
        peer: Option<std::net::SocketAddr>,
    ) -> Box<dyn DataSession> {
        Box::new(WsDataSession {
            io: Box::new(AxumWs::new(socket)),
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
            "ws 服务端请使用 WsTransport::from_upgraded（HTTP 升级）",
        ))
    }

    fn stats(&self) -> TransportStats {
        self.stats.as_ref().clone()
    }
}

/// WS 线上消息（两种 socket 的统一视图）。
enum WsMsg {
    Text(String),
    Binary(Bytes),
}

/// 底层 WS 通道适配（axum 服务端 / tungstenite 客户端各一个实现）。
#[async_trait]
trait WsIo: Send + Sync + 'static {
    async fn send_msg(&self, msg: WsMsg) -> Result<(), TransportError>;
    /// `Ok(None)` 表示连接已关闭（Close 帧或 EOF）。
    async fn recv_msg(&self) -> Result<Option<WsMsg>, TransportError>;
    async fn close(&self) -> Result<(), TransportError>;
}

/// axum 服务端 socket 适配。
struct AxumWs {
    inner: Mutex<Option<axum::extract::ws::WebSocket>>,
}

impl AxumWs {
    fn new(socket: axum::extract::ws::WebSocket) -> Self {
        Self {
            inner: Mutex::new(Some(socket)),
        }
    }
}

#[async_trait]
impl WsIo for AxumWs {
    async fn send_msg(&self, msg: WsMsg) -> Result<(), TransportError> {
        use axum::extract::ws::Message as M;
        let msg = match msg {
            WsMsg::Text(s) => M::Text(s.into()),
            WsMsg::Binary(b) => M::Binary(b),
        };
        let mut guard = self.inner.lock().await;
        let socket = guard.as_mut().ok_or(TransportError::Closed)?;
        socket
            .send(msg)
            .await
            .map_err(|e| TransportError::Io(e.to_string()))
    }

    async fn recv_msg(&self) -> Result<Option<WsMsg>, TransportError> {
        use axum::extract::ws::Message as M;
        loop {
            let mut guard = self.inner.lock().await;
            let socket = guard.as_mut().ok_or(TransportError::Closed)?;
            match socket.recv().await {
                Some(Ok(M::Text(t))) => return Ok(Some(WsMsg::Text(t.to_string()))),
                Some(Ok(M::Binary(b))) => return Ok(Some(WsMsg::Binary(b))),
                Some(Ok(M::Close(_))) | None => return Ok(None),
                Some(Ok(M::Ping(_)) | Ok(M::Pong(_))) => continue, // 不主动发 ping，忽略
                Some(Err(e)) => return Err(TransportError::Io(e.to_string())),
            }
        }
    }

    async fn close(&self) -> Result<(), TransportError> {
        let mut guard = self.inner.lock().await;
        if let Some(mut socket) = guard.take() {
            let _ = socket.send(axum::extract::ws::Message::Close(None)).await;
        }
        Ok(())
    }
}

/// tungstenite 客户端 socket 适配。
struct TungsteniteWs {
    inner: Mutex<
        Option<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>>,
    >,
}

impl TungsteniteWs {
    fn new(
        socket: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
    ) -> Self {
        Self {
            inner: Mutex::new(Some(socket)),
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
        let mut guard = self.inner.lock().await;
        let socket = guard.as_mut().ok_or(TransportError::Closed)?;
        socket
            .send(msg)
            .await
            .map_err(|e| TransportError::Io(e.to_string()))
    }

    async fn recv_msg(&self) -> Result<Option<WsMsg>, TransportError> {
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::Message as M;
        loop {
            let mut guard = self.inner.lock().await;
            let socket = guard.as_mut().ok_or(TransportError::Closed)?;
            match socket.next().await {
                Some(Ok(M::Text(t))) => return Ok(Some(WsMsg::Text(t.to_string()))),
                Some(Ok(M::Binary(b))) => return Ok(Some(WsMsg::Binary(b))),
                Some(Ok(M::Close(_))) | None => return Ok(None),
                Some(Ok(M::Ping(_)) | Ok(M::Pong(_)) | Ok(M::Frame(_))) => continue,
                Some(Err(e)) => return Err(TransportError::Io(e.to_string())),
            }
        }
    }

    async fn close(&self) -> Result<(), TransportError> {
        use futures_util::SinkExt;
        let mut guard = self.inner.lock().await;
        if let Some(mut socket) = guard.take() {
            let _ = socket
                .send(tokio_tungstenite::tungstenite::Message::Close(None))
                .await;
        }
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
