//! # stross-transport —— 可插拔传输层（设计文档 docs/plugin-architecture.md §4）
//!
//! 内核与上层只看到 [`Transport`] / [`DataSession`]，不关心具体线格式：
//!
//! * [`ws::WsTransport`]：WebSocket 传输（现状，无损，控制通道 + 媒体兜底）
//! * [`webrtc::WebRtcTransport`]：WebRTC 传输（有损低延迟；信令经 HTTP 建立）
//! * [`srt::SrtTransport`]：SRT 传输（自适应，rsrt 纯 Rust；弱网/跨 NAT 推流）
//! * [`quic::QuicTransport`]：QUIC 传输（无损，quinn；一条连接 control/media 多路复用）
//! * [`memory::MemoryTransport`]：内存传输（测试 / 示例用）
//!
//! 传输实现负责把 [`SessionPacket`] 映射到具体线格式——分片/重组是传输实现的
//! 内部事务（UDP 类传输用帧头的 `frag_*` 字段切大关键帧；WS/QUIC 整帧发送）。
//!
//! [`net`]：本机局域网 IP（原 `stross_core::net`，随传输层下沉）。

pub mod memory;
pub mod net;
pub mod quic;
pub mod relay_url;
pub mod srt;
pub mod webrtc;
pub mod ws;

pub use memory::{BufferPool, BytesChunks, chunk_bytes};
pub use relay_url::RelayUrl;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use stross_proto::frame::Frame;
use stross_proto::message::{ControlMessage, ReliabilityProfile, TransportId};
use thiserror::Error;

/// 传输统计数据（供内核事件 / 观看端 stats UI 使用）。
///
/// 字节计数为原子（`fetch_add`），媒体热路径（每帧）只做无锁累加；
/// rtt/loss/jitter 为低频可选项，普通字段即可。
#[derive(Debug, Default)]
pub struct TransportStats {
    pub rtt_ms: Option<u32>,
    pub loss_pct: Option<f32>,
    pub jitter_ms: Option<f32>,
    bytes_sent: AtomicU64,
    bytes_recv: AtomicU64,
}

impl TransportStats {
    /// 已发送字节数（原子读）。
    pub fn bytes_sent(&self) -> u64 {
        self.bytes_sent.load(Ordering::Relaxed)
    }

    /// 已接收字节数（原子读）。
    pub fn bytes_recv(&self) -> u64 {
        self.bytes_recv.load(Ordering::Relaxed)
    }

    /// 累加已发送字节（热路径：无锁）。
    pub(crate) fn add_sent(&self, n: usize) {
        self.bytes_sent.fetch_add(n as u64, Ordering::Relaxed);
    }

    /// 累加已接收字节（热路径：无锁）。
    pub(crate) fn add_recv(&self, n: usize) {
        self.bytes_recv.fetch_add(n as u64, Ordering::Relaxed);
    }
}

impl Clone for TransportStats {
    fn clone(&self) -> Self {
        Self {
            rtt_ms: self.rtt_ms,
            loss_pct: self.loss_pct,
            jitter_ms: self.jitter_ms,
            bytes_sent: AtomicU64::new(self.bytes_sent()),
            bytes_recv: AtomicU64::new(self.bytes_recv()),
        }
    }
}

impl serde::Serialize for TransportStats {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("TransportStats", 5)?;
        st.serialize_field("rttMs", &self.rtt_ms)?;
        st.serialize_field("lossPct", &self.loss_pct)?;
        st.serialize_field("jitterMs", &self.jitter_ms)?;
        st.serialize_field("bytesSent", &self.bytes_sent())?;
        st.serialize_field("bytesRecv", &self.bytes_recv())?;
        st.end()
    }
}

/// 会话建立参数。
#[derive(Debug, Clone)]
pub struct SessionParams {
    pub session_id: String,
    pub profile: ReliabilityProfile,
}

/// 按 relay URL scheme 选传输实现（推流 `RelayClient` / 观看 `connect_watch`
/// 共用，避免各调用点重复 if-else；可靠性契约由 [`Transport::profile`] 给出）。
///
/// scheme 判定收口在 [`RelayUrl`]：`srt://` → SRT，`quic://` → QUIC，
/// 其余（`ws://` / `wss://` / 无法解析）→ WS。
pub fn transport_for_url(url: &str) -> Box<dyn Transport> {
    match RelayUrl::parse(url).map(|u| u.transport()) {
        Some(TransportId::Srt) => Box::new(srt::SrtTransport::new()),
        Some(TransportId::Quic) => Box::new(quic::QuicTransport::new()),
        _ => Box::new(ws::WsTransport::new()),
    }
}

/// 对端地址。
#[derive(Debug, Clone)]
pub struct PeerAddr {
    pub transport: TransportId,
    pub addr: String,
}

/// 会话上的数据包：控制消息或媒体帧。
#[derive(Debug, Clone)]
pub enum SessionPacket {
    Control(ControlMessage),
    Media(Frame),
}

/// 传输错误。
#[derive(Debug, Error)]
pub enum TransportError {
    #[error("传输 {0} 不支持该操作")]
    NotSupported(&'static str),
    #[error("连接失败: {0}")]
    Connect(String),
    #[error("IO 错误: {0}")]
    Io(String),
    #[error("协议错误: {0}")]
    Protocol(String),
    #[error("连接已关闭")]
    Closed,
}

/// 可插拔传输。每个实现是一个无状态工厂 + 共享统计；会话由 [`DataSession`] 表示。
#[async_trait]
pub trait Transport: Send + Sync + 'static {
    /// 传输 id（[`TransportId`]）。
    fn id(&self) -> TransportId;
    /// 可靠性契约。
    fn profile(&self) -> ReliabilityProfile;

    /// 发起方：连接对端并建立数据会话。
    async fn connect(
        &self,
        peer: &PeerAddr,
        params: &SessionParams,
    ) -> Result<Box<dyn DataSession>, TransportError>;

    /// 接收方：接受一个入站会话。
    ///
    /// 仅面向监听型传输（TCP / QUIC / 内存）；WS 服务端从 HTTP 升级处
    /// 直接构造会话（见 [`ws::WsTransport::from_upgraded`]），本方法返回
    /// [`TransportError::NotSupported`]。
    async fn accept(&self, params: &SessionParams) -> Result<Box<dyn DataSession>, TransportError>;

    /// 当前传输统计（所有会话累计）。
    fn stats(&self) -> TransportStats;
}

/// 一条已建立的传输会话。
#[async_trait]
pub trait DataSession: Send + Sync + 'static {
    /// 发送一个包（控制消息或媒体帧）。
    async fn send(&self, pkt: SessionPacket) -> Result<(), TransportError>;
    /// 接收一个包；`Ok(None)` 表示对端已干净关闭。
    async fn recv(&self) -> Result<Option<SessionPacket>, TransportError>;
    /// 关闭会话。
    async fn close(&self) -> Result<(), TransportError>;
    /// 对端地址（来源感知门控用：回环 = 本机流程走内核预授权；
    /// 非回环 / 未知 = 跨设备接入，必须出示接入凭证）。
    fn peer_addr(&self) -> Option<std::net::SocketAddr> {
        None
    }
}

/// 共享统计计数器（Transport 与其派生的会话共享；字节计数为原子，无锁）。
pub(crate) type SharedStats = Arc<TransportStats>;
