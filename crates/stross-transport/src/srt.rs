//! SRT 传输实现（rsrt 0.3，纯 Rust；设计文档 docs/plugin-architecture.md §4.4）。
//!
//! SRT（Secure Reliable Transport）是 UDP 之上的可靠实时传输：TSBPD（时基
//! 包投递）+ ARQ（重传）+ too-late 丢包，即 [`ReliabilityProfile::Adaptive`]。
//! 弱网/跨 NAT 场景下比 WS（TCP）与裸 WebRTC 更稳。
//!
//! 消费方是原生推流端（浏览器无法直连 SRT）：relay 侧用 [`SrtTransport::bind`]
//! 监听，推流端用 [`Transport::connect`] 拨号 `srt://host:port`；数据面复用
//! 同一套 [`handle_push`](stross_core::relay) 逻辑（传输抽象第三次验证）。
//!
//! ## 线格式（每个 SRT 消息 = 1 字节类型 + 载荷）
//!
//! * `0x00 Control`：载荷 = JSON 控制消息文本（≤ [`FRAGMENT_LEN`]，握手/协商级）
//! * `0x01 Media`：载荷 = 24 字节 v2 帧头 + 片载荷 —— 超过 [`FRAGMENT_LEN`]
//!   的大帧按帧头 `frag_idx/frag_cnt` 分片（SRT 单消息上限 = 协商 MSS−44，
//!   默认 1500→1456；1080p 关键帧远大于此，必须分片）
//!
//! 分片/重组是传输实现的内部事务（设计文档 §4.2）；上层永远只看到完整
//! [`Frame`]。TSBPD 有序交付，重组只需按 `frag_idx` 累积，缺片即弃整帧
//! （丢帧自愈，等下一个关键帧）。

use std::net::SocketAddrV4;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Mutex;

use stross_proto::frame::{Frame, FrameHeader, HEADER_LEN};
use stross_proto::message::{ControlMessage, ReliabilityProfile, TransportId};

use super::{
    DataSession, PeerAddr, SessionPacket, SessionParams, SharedStats, Transport, TransportError,
    TransportStats,
};

/// SRT 单消息载荷上限（保守取 1400 < 默认 MSS−44=1456，兼容对端更小 MSS）。
pub const FRAGMENT_LEN: usize = 1400;

/// SRT 消息类型前缀。
#[repr(u8)]
enum PktType {
    Control = 0x00,
    Media = 0x01,
}

/// SRT 传输（Adaptive profile）。
pub struct SrtTransport {
    stats: SharedStats,
}

impl Default for SrtTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl SrtTransport {
    pub fn new() -> Self {
        Self {
            stats: Arc::new(TransportStats::default()),
        }
    }

    /// relay 侧：绑定 SRT 监听地址（`0.0.0.0:0` 随机端口）。
    ///
    /// 返回可重复 accept 的句柄；每个入站连接由调用方 spawn 处理
    /// （relay 里即 `handle_push`）。
    pub async fn bind(
        &self,
        bind: impl tokio::net::ToSocketAddrs,
    ) -> Result<SrtListenerHandle, TransportError> {
        let listener = rsrt::SrtListener::bind(bind, rsrt::SrtOptions::default())
            .await
            .map_err(|e| TransportError::Io(format!("SRT 监听失败: {e}")))?;
        Ok(SrtListenerHandle {
            listener,
            stats: self.stats.clone(),
        })
    }
}

#[async_trait]
impl Transport for SrtTransport {
    fn id(&self) -> TransportId {
        TransportId::Srt
    }

    fn profile(&self) -> ReliabilityProfile {
        ReliabilityProfile::Adaptive
    }

    async fn connect(
        &self,
        peer: &PeerAddr,
        _params: &SessionParams,
    ) -> Result<Box<dyn DataSession>, TransportError> {
        let addr = peer.addr.strip_prefix("srt://").unwrap_or(&peer.addr);
        let sock = rsrt::SrtSocket::connect(addr, rsrt::SrtOptions::default())
            .await
            .map_err(|e| TransportError::Connect(format!("SRT 连接失败: {e}")))?;
        tracing::info!("SRT 已连接: {addr}");
        Ok(Box::new(SrtDataSession::new(
            sock,
            self.stats.clone(),
            addr.parse().ok(),
        )))
    }

    async fn accept(
        &self,
        _params: &SessionParams,
    ) -> Result<Box<dyn DataSession>, TransportError> {
        Err(TransportError::NotSupported(
            "srt 服务端请使用 SrtTransport::bind + SrtListenerHandle::accept",
        ))
    }

    fn stats(&self) -> TransportStats {
        self.stats.as_ref().clone()
    }
}

/// 已绑定的 SRT 监听句柄（relay 持有，重复 accept）。
pub struct SrtListenerHandle {
    listener: rsrt::SrtListener,
    stats: SharedStats,
}

impl SrtListenerHandle {
    /// 本地监听地址（供展示/测试）。
    pub fn local_addr(&self) -> SocketAddrV4 {
        self.listener.local_addr()
    }

    /// 接受一个入站连接并包装成数据会话。
    pub async fn accept(&mut self) -> Result<Box<dyn DataSession>, TransportError> {
        let (sock, peer) = self
            .listener
            .accept()
            .await
            .map_err(|e| TransportError::Io(format!("SRT accept 失败: {e}")))?;
        tracing::info!("SRT 入站连接: {peer}");
        Ok(Box::new(SrtDataSession::new(
            sock,
            self.stats.clone(),
            Some(peer.into()),
        )))
    }
}

/// SRT 数据会话：1B 类型前缀 + 分片/重组。
pub struct SrtDataSession {
    sock: Mutex<Option<rsrt::SrtSocket>>,
    stats: SharedStats,
    rx: Mutex<RxState>,
    /// 对端地址（来源感知门控；回环 = 本机，非回环 = 凭证接入）。
    peer: Option<std::net::SocketAddr>,
}

/// 接收侧重组状态（TSBPD 有序 → 只按 `frag_idx` 累积）。
#[derive(Default)]
struct RxState {
    /// 当前分片帧（`None` = 无未完成帧）。
    pending: Option<PendingFrame>,
}

struct PendingFrame {
    header: FrameHeader,
    frags: Vec<Bytes>,
    next: u8,
}

impl SrtDataSession {
    pub fn new(
        sock: rsrt::SrtSocket,
        stats: SharedStats,
        peer: Option<std::net::SocketAddr>,
    ) -> Self {
        Self {
            sock: Mutex::new(Some(sock)),
            stats,
            rx: Mutex::new(RxState::default()),
            peer,
        }
    }
}

#[async_trait]
impl DataSession for SrtDataSession {
    fn peer_addr(&self) -> Option<std::net::SocketAddr> {
        self.peer
    }
    async fn send(&self, pkt: SessionPacket) -> Result<(), TransportError> {
        let guard = self.sock.lock().await;
        let sock = guard.as_ref().ok_or(TransportError::Closed)?;
        match pkt {
            SessionPacket::Control(c) => {
                let text = c.to_text();
                if text.len() > FRAGMENT_LEN {
                    return Err(TransportError::Protocol(format!(
                        "控制消息过大（{} > {FRAGMENT_LEN} 字节）",
                        text.len()
                    )));
                }
                let mut msg = Vec::with_capacity(1 + text.len());
                msg.push(PktType::Control as u8);
                msg.extend_from_slice(text.as_bytes());
                let n = msg.len();
                sock.send(&msg)
                    .await
                    .map_err(|e| TransportError::Io(format!("SRT 发送失败: {e}")))?;
                self.stats.add_sent(n);
            }
            SessionPacket::Media(frame) => {
                let payload = &frame.payload;
                // rsrt::send 内部会再 to_vec 一次；这里复用一块 buffer
                // 避免每帧/每片重复分配
                let mut msg = Vec::with_capacity(1 + HEADER_LEN + FRAGMENT_LEN);
                if payload.len() <= FRAGMENT_LEN {
                    msg.push(PktType::Media as u8);
                    msg.extend_from_slice(&frame.header.encode());
                    msg.extend_from_slice(payload);
                    let n = msg.len();
                    sock.send(&msg)
                        .await
                        .map_err(|e| TransportError::Io(format!("SRT 发送失败: {e}")))?;
                    self.stats.add_sent(n);
                } else {
                    // 分片：每片 = 1B 类型 + 帧头（frag_* 标记）+ 片载荷
                    // （1080p 关键帧可达数百片）
                    let frag_cnt = (payload.len().div_ceil(FRAGMENT_LEN)) as u8;
                    for (i, chunk) in payload.chunks(FRAGMENT_LEN).enumerate() {
                        let mut header = frame.header;
                        header.frag_idx = i as u8;
                        header.frag_cnt = frag_cnt;
                        header.len = chunk.len() as u32;
                        msg.clear();
                        msg.push(PktType::Media as u8);
                        msg.extend_from_slice(&header.encode());
                        msg.extend_from_slice(chunk);
                        let n = msg.len();
                        sock.send(&msg)
                            .await
                            .map_err(|e| TransportError::Io(format!("SRT 发送失败: {e}")))?;
                        self.stats.add_sent(n);
                    }
                }
            }
        }
        Ok(())
    }

    async fn recv(&self) -> Result<Option<SessionPacket>, TransportError> {
        let mut guard = self.sock.lock().await;
        let sock = guard.as_mut().ok_or(TransportError::Closed)?;
        loop {
            let Some(bytes) = sock
                .recv()
                .await
                .map_err(|e| TransportError::Io(format!("SRT 接收失败: {e}")))?
            else {
                return Ok(None); // 对端干净关闭（SRT SHUTDOWN）
            };
            self.stats.add_recv(bytes.len());
            if let Some(pkt) = decode_message(bytes, &mut *self.rx.lock().await) {
                return Ok(Some(pkt));
            }
            // `None` = 分片累积中（或一条损坏消息）：继续收下一片/下一条
        }
    }

    async fn close(&self) -> Result<(), TransportError> {
        let sock = self.sock.lock().await.take();
        if let Some(sock) = sock {
            let _ = sock.close().await;
        }
        Ok(())
    }
}

/// 解码一个 SRT 消息；媒体分片在此重组。
///
/// 零拷贝：消息是传输层读入的 `Bytes`，未分片帧载荷直接切片共享；
/// 分片帧的片载荷也切片累积，仅最终拼接为连续缓冲区时复制一次。
fn decode_message(bytes: Bytes, rx: &mut RxState) -> Option<SessionPacket> {
    let ty = *bytes.first()?;
    match ty {
        // 与 PktType::Control / PktType::Media 对应（#[repr(u8)]）
        0x00 => {
            let text = std::str::from_utf8(&bytes[1..]).ok()?;
            ControlMessage::from_text(text)
                .ok()
                .map(SessionPacket::Control)
        }
        0x01 => {
            let header = FrameHeader::decode(&bytes[1..]).ok()?;
            let payload = bytes.slice(1 + HEADER_LEN..);
            if header.frag_cnt == 0 {
                Some(SessionPacket::Media(Frame { header, payload }))
            } else {
                rx.push_fragment(header, payload)
            }
        }
        _ => None,
    }
}

impl RxState {
    /// 累积一个分片；整帧齐了返回完整帧，否则 `None`。
    fn push_fragment(&mut self, header: FrameHeader, payload: Bytes) -> Option<SessionPacket> {
        match &mut self.pending {
            Some(p) if p.header.seq == header.seq && header.frag_idx == p.next => {
                p.frags.push(payload);
                p.next += 1;
                if p.next == header.frag_cnt {
                    let p = self.pending.take().unwrap();
                    let total: usize = p.frags.iter().map(|b| b.len()).sum();
                    let mut h = p.header;
                    h.frag_idx = 0;
                    h.frag_cnt = 0;
                    h.len = total as u32;
                    let mut out = Vec::with_capacity(total);
                    for f in p.frags {
                        out.extend_from_slice(&f);
                    }
                    Some(SessionPacket::Media(Frame {
                        header: h,
                        payload: out.into(),
                    }))
                } else {
                    None
                }
            }
            // 缺片/乱序：弃旧帧；仅 frag_idx==0 开启新帧（丢帧自愈）
            _ => {
                if header.frag_idx == 0 {
                    self.pending = Some(PendingFrame {
                        header,
                        frags: vec![payload],
                        next: 1,
                    });
                } else {
                    self.pending = None;
                }
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stross_proto::frame::{CODEC_H264, FLAG_KEYFRAME, TRACK_VIDEO};

    /// 真 UDP loopback 上的 SRT 分片/重组 roundtrip。
    #[tokio::test]
    async fn srt_fragment_roundtrip() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "warn".into()),
            )
            .with_test_writer()
            .try_init();
        let transport = SrtTransport::new();
        let mut handle = transport.bind("127.0.0.1:0").await.unwrap();
        let addr = handle.local_addr();
        let accept_task = tokio::spawn(async move { handle.accept().await.unwrap() });

        let peer = PeerAddr {
            transport: TransportId::Srt,
            addr: format!("srt://127.0.0.1:{}", addr.port()),
        };
        let params = SessionParams {
            session_id: "s1".into(),
            profile: ReliabilityProfile::Adaptive,
        };
        let client = transport.connect(&peer, &params).await.unwrap();
        let server = accept_task.await.unwrap();

        // 控制消息（Hello）
        client
            .send(SessionPacket::Control(ControlMessage::Hello {
                stream_id: "s1".into(),
                title: "t".into(),
                video: None,
                audio: None,
                share_token: None,
            }))
            .await
            .unwrap();
        let pkt = server.recv().await.unwrap().unwrap();
        assert!(matches!(
            pkt,
            SessionPacket::Control(ControlMessage::Hello { .. })
        ));

        // 大关键帧（> FRAGMENT_LEN，触发分片）→ 重组后逐字节一致
        let big: Vec<u8> = (0..(FRAGMENT_LEN * 3 + 17))
            .map(|i| (i % 251) as u8)
            .collect();
        client
            .send(SessionPacket::Media(Frame::new(
                TRACK_VIDEO,
                CODEC_H264,
                FLAG_KEYFRAME,
                123,
                big.clone(),
            )))
            .await
            .unwrap();
        let pkt = server.recv().await.unwrap().unwrap();
        match pkt {
            SessionPacket::Media(frame) => {
                assert_eq!(frame.payload.len(), big.len());
                assert_eq!(frame.payload.to_vec(), big);
                assert_eq!(frame.header.pts_ms, 123);
                assert!(frame.header.is_keyframe());
                assert!(frame.header.is_whole(), "重组后应恢复未分片标记");
            }
            other => panic!("期望 Media，得到 {other:?}"),
        }

        // 恰好边界：整帧 = FRAGMENT_LEN 不分片
        let exact = vec![7u8; FRAGMENT_LEN];
        client
            .send(SessionPacket::Media(Frame::new(
                TRACK_VIDEO,
                CODEC_H264,
                0,
                456,
                exact.clone(),
            )))
            .await
            .unwrap();
        let pkt = server.recv().await.unwrap().unwrap();
        match pkt {
            SessionPacket::Media(frame) => {
                assert_eq!(frame.payload.to_vec(), exact);
                assert_eq!(frame.header.pts_ms, 456);
            }
            other => panic!("期望 Media，得到 {other:?}"),
        }

        client.close().await.unwrap();
        assert!(server.recv().await.unwrap().is_none());
    }

    #[test]
    fn profile_is_adaptive() {
        assert_eq!(SrtTransport::new().profile(), ReliabilityProfile::Adaptive);
        assert_eq!(SrtTransport::new().id(), TransportId::Srt);
    }
}
