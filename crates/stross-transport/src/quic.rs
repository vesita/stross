//! QUIC 传输实现（quinn 0.11 + rustls-ring；设计文档 docs/plugin-architecture.md §4.4）。
//!
//! QUIC 是 Lossless 契约：一条连接**多路复用**两条双向 stream（控制/媒体分离，
//! 未来 input/剪贴板可再加一条，NAT 友好、0-RTT 重连）。消费方是原生推流端
//! （浏览器 http 源无法直连 QUIC/WebTransport）：
//!
//! * relay 侧：[`QuicTransport::bind`] 监听（自签名证书，进程内一次生成）
//! * 推流端：[`Transport::connect`] 拨号 `quic://host:port`（danger 接受自签名）
//!
//! ## 线格式（每条 stream 独立，stream 即类型，无需消息类型前缀）
//!
//! * stream 0 = control：消息 = `u32 LE 长度` + JSON 控制消息文本
//! * stream 1 = media：消息 = `u32 LE 长度` + 24 字节 v2 帧头 + 载荷
//!
//! QUIC stream 是可靠字节流（无消息边界、无单消息大小限制）——长度前缀分帧；
//! 大关键帧整体发送，**不需要** v2 头的 `frag_*` 分片（与 WS 一致）。
//!
//! 安全模型：自签名 TLS + 客户端接受任意证书——与明文 `ws://` 同级（局域网
//! 可信模型）。加密仍在（QUIC 强制 TLS），只是不验证身份。

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use bytes::Bytes;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::sync::Mutex;

use stross_proto::frame::Frame;
use stross_proto::message::{ControlMessage, ReliabilityProfile, TransportId};

use super::{
    DataSession, PeerAddr, SessionPacket, SessionParams, SharedStats, Transport, TransportError,
    TransportStats,
};

/// 长度前缀分帧：`u32 LE` 消息长度 + 载荷。
const LEN_BYTES: usize = 4;

/// QUIC 传输（Lossless profile，多路复用）。
pub struct QuicTransport {
    stats: SharedStats,
}

impl Default for QuicTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl QuicTransport {
    pub fn new() -> Self {
        Self {
            stats: Arc::new(TransportStats::default()),
        }
    }

    /// relay 侧：绑定 QUIC 监听（随机端口；`0.0.0.0:0`）。
    pub async fn bind(&self, bind: SocketAddr) -> Result<QuicListenerHandle, TransportError> {
        let endpoint = quinn::Endpoint::server(server_config()?, bind)
            .map_err(|e| TransportError::Io(format!("QUIC 监听失败: {e}")))?;
        Ok(QuicListenerHandle {
            endpoint,
            stats: self.stats.clone(),
        })
    }
}

#[async_trait]
impl Transport for QuicTransport {
    fn id(&self) -> TransportId {
        TransportId::Quic
    }

    fn profile(&self) -> ReliabilityProfile {
        ReliabilityProfile::Lossless
    }

    async fn connect(
        &self,
        peer: &PeerAddr,
        _params: &SessionParams,
    ) -> Result<Box<dyn DataSession>, TransportError> {
        let addr: SocketAddr = peer
            .addr
            .strip_prefix("quic://")
            .unwrap_or(&peer.addr)
            .parse()
            .map_err(|e| TransportError::Connect(format!("QUIC 地址解析失败: {e}")))?;
        let endpoint = quinn::Endpoint::client("0.0.0.0:0".parse().expect("静态地址"))
            .map_err(|e| TransportError::Io(e.to_string()))?;
        let connecting = endpoint
            .connect_with(client_config()?, addr, "stross")
            .map_err(|e| TransportError::Connect(format!("QUIC 连接失败: {e}")))?;
        let connection = connecting
            .await
            .map_err(|e| TransportError::Connect(format!("QUIC 握手失败: {e}")))?;
        // 约定：客户端先开 control stream，再开 media stream。
        // QUIC 流是 lazy 的：open_bi 只分配流 ID，对端 accept_bi 要等首个
        // STREAM 帧——立即各发一条空消息（长度 0）作为「流就绪」信号，
        // 服务端 accept 才能及时返回；读端会跳过空消息。
        let (mut control_tx, control_rx) = connection
            .open_bi()
            .await
            .map_err(|e| TransportError::Connect(format!("QUIC 开 control stream 失败: {e}")))?;
        control_tx
            .write_all(&0u32.to_le_bytes())
            .await
            .map_err(|e| TransportError::Connect(format!("QUIC control 就绪信号失败: {e}")))?;
        let (mut media_tx, media_rx) = connection
            .open_bi()
            .await
            .map_err(|e| TransportError::Connect(format!("QUIC 开 media stream 失败: {e}")))?;
        media_tx
            .write_all(&0u32.to_le_bytes())
            .await
            .map_err(|e| TransportError::Connect(format!("QUIC media 就绪信号失败: {e}")))?;
        tracing::info!("QUIC 已连接: {addr}");
        Ok(Box::new(QuicDataSession::new(
            Some(endpoint), // 客户端持有以保持连接存活
            connection,
            control_tx,
            control_rx,
            media_tx,
            media_rx,
            self.stats.clone(),
        )))
    }

    async fn accept(
        &self,
        _params: &SessionParams,
    ) -> Result<Box<dyn DataSession>, TransportError> {
        Err(TransportError::NotSupported(
            "quic 服务端请使用 QuicTransport::bind + QuicListenerHandle::accept",
        ))
    }

    fn stats(&self) -> TransportStats {
        self.stats.as_ref().clone()
    }
}

/// 已绑定的 QUIC 监听句柄。
pub struct QuicListenerHandle {
    endpoint: quinn::Endpoint,
    stats: SharedStats,
}

impl QuicListenerHandle {
    /// 本地监听地址。
    pub fn local_addr(&self) -> SocketAddr {
        self.endpoint.local_addr().expect("已绑定端点")
    }

    /// 接受一个入站连接：等客户端开 control/media 两条 bi stream，包装成会话。
    pub async fn accept(&mut self) -> Result<Box<dyn DataSession>, TransportError> {
        let Some(incoming) = self.endpoint.accept().await else {
            return Err(TransportError::Closed);
        };
        let connecting = incoming
            .accept()
            .map_err(|e| TransportError::Io(format!("QUIC accept 失败: {e}")))?;
        let connection = connecting
            .await
            .map_err(|e| TransportError::Io(format!("QUIC 握手失败: {e}")))?;
        let peer = connection.remote_address();
        let (control_tx, control_rx) = connection
            .accept_bi()
            .await
            .map_err(|e| TransportError::Io(format!("QUIC 收 control stream 失败: {e}")))?;
        let (media_tx, media_rx) = connection
            .accept_bi()
            .await
            .map_err(|e| TransportError::Io(format!("QUIC 收 media stream 失败: {e}")))?;
        tracing::info!("QUIC 入站连接: {peer}");
        Ok(Box::new(QuicDataSession::new(
            None, // 服务端 endpoint 由 listener handle 持有
            connection,
            control_tx,
            control_rx,
            media_tx,
            media_rx,
            self.stats.clone(),
        )))
    }
}

/// QUIC 数据会话：control/media 双 stream 多路复用。
pub struct QuicDataSession {
    /// 客户端持有以保持连接存活（Endpoint drop 会关闭连接）；服务端由
    /// `QuicListenerHandle` 持有，此处为 `None`。
    _endpoint: Option<quinn::Endpoint>,
    conn: quinn::Connection,
    control_tx: Mutex<quinn::SendStream>,
    control_rx: Mutex<quinn::RecvStream>,
    media_tx: Mutex<quinn::SendStream>,
    media_rx: Mutex<quinn::RecvStream>,
    stats: SharedStats,
}

impl QuicDataSession {
    pub fn new(
        endpoint: Option<quinn::Endpoint>,
        conn: quinn::Connection,
        control_tx: quinn::SendStream,
        control_rx: quinn::RecvStream,
        media_tx: quinn::SendStream,
        media_rx: quinn::RecvStream,
        stats: SharedStats,
    ) -> Self {
        Self {
            _endpoint: endpoint,
            conn,
            control_tx: Mutex::new(control_tx),
            control_rx: Mutex::new(control_rx),
            media_tx: Mutex::new(media_tx),
            media_rx: Mutex::new(media_rx),
            stats,
        }
    }
}

#[async_trait]
impl DataSession for QuicDataSession {
    fn peer_addr(&self) -> Option<std::net::SocketAddr> {
        Some(self.conn.remote_address())
    }

    async fn send(&self, pkt: SessionPacket) -> Result<(), TransportError> {
        match pkt {
            SessionPacket::Control(c) => {
                let text = c.to_text();
                let mut tx = self.control_tx.lock().await;
                write_msg(&mut tx, text.as_bytes()).await?;
                self.stats.add_sent(LEN_BYTES + text.len());
            }
            SessionPacket::Media(frame) => {
                let full = frame.to_bytes();
                let mut tx = self.media_tx.lock().await;
                write_msg(&mut tx, &full).await?;
                self.stats.add_sent(LEN_BYTES + full.len());
            }
        }
        Ok(())
    }

    async fn recv(&self) -> Result<Option<SessionPacket>, TransportError> {
        // 两个分支都直接返回（控制流优先），无需外层 loop
        tokio::select! {
            // 偏向 control：控制消息（Welcome/Error）优先处理
            biased;
            r = self.recv_control() => {
                let Some(bytes) = r? else { return Ok(None) };
                self.stats.add_recv(LEN_BYTES + bytes.len());
                let text = std::str::from_utf8(&bytes)
                    .map_err(|e| TransportError::Protocol(e.to_string()))?;
                let msg = ControlMessage::from_text(text)
                    .map_err(|e| TransportError::Protocol(e.to_string()))?;
                Ok(Some(SessionPacket::Control(msg)))
            }
            r = self.recv_media() => {
                let Some(bytes) = r? else { return Ok(None) };
                self.stats.add_recv(LEN_BYTES + bytes.len());
                let frame = Frame::from_bytes_owned(bytes)
                    .map_err(|e| TransportError::Protocol(e.to_string()))?;
                Ok(Some(SessionPacket::Media(frame)))
            }
        }
    }

    async fn close(&self) -> Result<(), TransportError> {
        self.conn.close(0u32.into(), b"bye");
        Ok(())
    }
}

impl QuicDataSession {
    /// 读 control stream 的一条真实消息（跳过空消息就绪信号；持锁跨 await）。
    async fn recv_control(&self) -> Result<Option<Bytes>, TransportError> {
        let mut guard = self.control_rx.lock().await;
        loop {
            match read_msg(&mut guard).await? {
                Some(b) if b.is_empty() => continue,
                other => return Ok(other),
            }
        }
    }

    /// 读 media stream 的一条真实消息。
    async fn recv_media(&self) -> Result<Option<Bytes>, TransportError> {
        let mut guard = self.media_rx.lock().await;
        loop {
            match read_msg(&mut guard).await? {
                Some(b) if b.is_empty() => continue,
                other => return Ok(other),
            }
        }
    }
}

/// 写一条长度前缀消息。
async fn write_msg(tx: &mut quinn::SendStream, payload: &[u8]) -> Result<(), TransportError> {
    let len = u32::try_from(payload.len())
        .map_err(|_| TransportError::Protocol("消息超过 4GiB".into()))?;
    tx.write_all(&len.to_le_bytes())
        .await
        .map_err(|e| TransportError::Io(format!("QUIC 写长度失败: {e}")))?;
    tx.write_all(payload)
        .await
        .map_err(|e| TransportError::Io(format!("QUIC 写载荷失败: {e}")))?;
    Ok(())
}

/// 读一条长度前缀消息；`Ok(None)` = 流干净结束（对端关闭该 stream）。
async fn read_msg(rx: &mut quinn::RecvStream) -> Result<Option<Bytes>, TransportError> {
    let mut len_buf = [0u8; LEN_BYTES];
    if let Err(e) = rx.read_exact(&mut len_buf).await {
        return map_read_err(e).map(|_| None);
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    if let Err(e) = rx.read_exact(&mut buf).await {
        return map_read_err(e).map(|_| None);
    }
    Ok(Some(buf.into()))
}

/// 把读错误映射为「干净结束」（对端 finish/reset/关闭连接）或真实错误。
fn map_read_err(e: quinn::ReadExactError) -> Result<bool, TransportError> {
    use quinn::{ConnectionError, ReadError, ReadExactError};
    match e {
        ReadExactError::FinishedEarly(_)
        | ReadExactError::ReadError(ReadError::Reset(_))
        | ReadExactError::ReadError(ReadError::ClosedStream)
        // 对端应用主动 close（或正常关连接）→ 视为干净结束
        | ReadExactError::ReadError(ReadError::ConnectionLost(
            ConnectionError::ApplicationClosed(_),
        ))
        | ReadExactError::ReadError(ReadError::ConnectionLost(
            ConnectionError::ConnectionClosed(_),
        )) => Ok(false),
        e => Err(TransportError::Io(format!("QUIC 读失败: {e}"))),
    }
}

// ---------------------------------------------------------------------------
// TLS：进程内一次生成的自签名证书（rcgen）+ 服务端/客户端配置
// ---------------------------------------------------------------------------

fn server_config() -> Result<quinn::ServerConfig, TransportError> {
    static CONFIG: OnceLock<Result<quinn::ServerConfig, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let certified = rcgen::generate_simple_self_signed(vec!["stross.local".into()])
                .map_err(|e| format!("自签名证书生成失败: {e}"))?;
            let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                certified.signing_key.serialize_der(),
            ));
            let cert = certified.cert.der().clone();
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = rustls::ServerConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map_err(|e| format!("TLS 协议版本配置失败: {e}"))?
                .with_no_client_auth()
                .with_single_cert(vec![cert], key)
                .map_err(|e| format!("TLS 证书装载失败: {e}"))?;
            let quic = quinn::crypto::rustls::QuicServerConfig::try_from(config)
                .map_err(|e| format!("QUIC 服务端配置失败: {e}"))?;
            Ok(quinn::ServerConfig::with_crypto(Arc::new(quic)))
        })
        .clone()
        .map_err(TransportError::Protocol)
}

/// 客户端：接受任意证书（局域网可信模型，与 ws:// 明文同级；加密仍在）。
fn client_config() -> Result<quinn::ClientConfig, TransportError> {
    static CONFIG: OnceLock<Result<quinn::ClientConfig, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map_err(|e| format!("TLS 协议版本配置失败: {e}"))?
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(SkipVerify))
                .with_no_client_auth();
            let quic = quinn::crypto::rustls::QuicClientConfig::try_from(config)
                .map_err(|e| format!("QUIC 客户端配置失败: {e}"))?;
            Ok(quinn::ClientConfig::new(Arc::new(quic)))
        })
        .clone()
        .map_err(TransportError::Protocol)
}

/// 自签名证书验证器：验证通过但**不**校验身份（签名算法仍校验）。
#[derive(Debug)]
struct SkipVerify;

impl rustls::client::danger::ServerCertVerifier for SkipVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stross_proto::frame::{CODEC_H264, FLAG_KEYFRAME, TRACK_VIDEO};

    /// 真 UDP loopback 上的 QUIC 双 stream 多路复用 roundtrip。
    #[tokio::test]
    async fn quic_multiplex_roundtrip() {
        let transport = QuicTransport::new();
        let mut handle = transport
            .bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = handle.local_addr();
        // 返回 handle 以保持服务端 endpoint 存活（与 relay 长期持有一致）
        let accept_task = tokio::spawn(async move {
            let session = handle.accept().await.unwrap();
            (session, handle)
        });

        let peer = PeerAddr {
            transport: TransportId::Quic,
            addr: format!("quic://127.0.0.1:{}", addr.port()),
        };
        let params = SessionParams {
            session_id: "q1".into(),
            profile: ReliabilityProfile::Lossless,
        };
        let client = transport.connect(&peer, &params).await.unwrap();
        let (server, _handle) = accept_task.await.unwrap();

        // 控制消息（Hello）
        client
            .send(SessionPacket::Control(ControlMessage::Hello {
                stream_id: "q1".into(),
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

        // 大关键帧：QUIC 无单消息大小限制，整体发送（无分片）→ 逐字节一致
        let big: Vec<u8> = (0..100_000).map(|i| (i % 251) as u8).collect();
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
            }
            other => panic!("期望 Media，得到 {other:?}"),
        }

        // 服务端 → 客户端：控制回执（反向路径验证）
        server
            .send(SessionPacket::Control(ControlMessage::Welcome {
                stream_id: "q1".into(),
            }))
            .await
            .unwrap();
        let pkt = client.recv().await.unwrap().unwrap();
        assert!(matches!(
            pkt,
            SessionPacket::Control(ControlMessage::Welcome { .. })
        ));

        client.close().await.unwrap();
        assert!(server.recv().await.unwrap().is_none());
    }

    #[test]
    fn profile_is_lossless() {
        assert_eq!(QuicTransport::new().profile(), ReliabilityProfile::Lossless);
        assert_eq!(QuicTransport::new().id(), TransportId::Quic);
    }
}
