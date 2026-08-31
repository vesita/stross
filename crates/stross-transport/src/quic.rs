//! QUIC 传输实现（quinn 0.11 + rustls-ring；设计文档 docs/plugin-architecture.md §4.4、
//! 通信模式 v2 docs/comm-mode-v2.md §5 Phase C「连接复用」）。
//!
//! QUIC 是 Lossless 契约。**v2（Phase C）：一条连接 = 一条节点间链路，承载 N 条媒体流**——
//! 替代 v1「每流一会话」（control/media 双 stream 只服务一条流）：
//!
//! * stream 0 = control：消息 = `u32 LE 长度` + JSON 控制消息文本
//!   （新增 [`ControlMessage::OpenStream`] / `StreamOpened` / `CloseStream`：
//!   流级登记 / 确认 / 拆解，互不级联）；
//! * stream 1..N = 媒体流：每条流一条 QUIC bi stream（stream 即类型，短 id 映射，
//!   docs/comm-mode-v2.md §6），消息 = `u32 LE 长度` + **v2 紧凑帧头**
//!   （14 字节：track/flags/pts/seq/len，见 [`stross_proto::frame::Frame2`]）
//!   + 载荷——codec 由 OpenStream 协商声明，接收侧按 track 路由即可。
//!
//! 客户端经**进程级链路管理器**（[`QUIC_LINKS`]）复用同 `(host, port)` 的连接：
//! 多个推流/观看会话共享一条连接（屏幕 + 系统声音同推/同收），停一条只拆该
//! 媒体流，不级联其它流。WS/SRT 保持每流独立连接（单流回退路径不受影响）。
//!
//! 安全模型：自签名 TLS + 客户端接受任意证书——与明文 `ws://` 同级（局域网
//! 可信模型）。加密仍在（QUIC 强制 TLS），只是不验证身份。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use bytes::Bytes;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::sync::{mpsc, Mutex};

use stross_proto::frame::{Frame, Frame2};
use stross_proto::message::{ControlMessage, ReliabilityProfile, StreamRole, TransportId};

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

    /// 客户端拨号：经**链路管理器**获取/建立到对端 `(host, port)` 的共享连接，
    /// 返回一条绑定到新开 QUIC bi stream 的 [`QuicMediaSession`]（首个控制消息
    /// Hello/Watch 被转换为 OpenStream 登记，媒体帧走 v2 紧凑帧头）。
    ///
    /// 上层（[`crate::sender::RelayClient`] / [`crate::watch::connect_watch`]）
    /// 无需感知复用：多路会话自然共享一条连接。
    async fn connect(
        &self,
        peer: &PeerAddr,
        _params: &SessionParams,
    ) -> Result<Box<dyn DataSession>, TransportError> {
        // 解析收口在 RelayUrl（去 scheme → host:port → SocketAddr）
        let addr: SocketAddr = super::RelayUrl::parse(&peer.addr)
            .and_then(|u| format!("{}:{}", u.host(), u.port()).parse().ok())
            .or_else(|| peer.addr.parse().ok())
            .ok_or_else(|| TransportError::Connect(format!("QUIC 地址解析失败: {}", peer.addr)))?;
        let link = link_for(addr).await?;
        tracing::debug!("QUIC 会话挂到共享连接 {addr}（复用）");
        Ok(Box::new(link.open_media().await))
    }

    async fn accept(
        &self,
        _params: &SessionParams,
    ) -> Result<Box<dyn DataSession>, TransportError> {
        Err(TransportError::NotSupported(
            "quic 服务端请使用 QuicTransport::bind + QuicListenerHandle::accept_link",
        ))
    }

    fn stats(&self) -> TransportStats {
        self.stats.as_ref().clone()
    }
}

// ---------------------------------------------------------------------------
// 客户端：共享连接（QuicLink）+ 媒体会话（QuicMediaSession）+ 链路管理器
// ---------------------------------------------------------------------------

/// 链路事件（control 循环路由到对应媒体会话）。
#[derive(Debug, Clone)]
enum LinkEvent {
    /// OpenStream 已被中继确认（上层转换为 Welcome / Ready）。
    Opened,
    /// 中继错误（OpenStream 被拒等）。
    Error(String),
    /// 中继关闭了该流（流结束 / 拆除）。
    Closed,
}

/// 一条到对端 `(host, port)` 的共享 QUIC 连接（Phase C 连接复用）。
///
/// * 生命周期：强引用只被媒体会话持有——全部会话关闭后 `QuicLink` drop →
///   quinn 连接关闭 → control 循环退出（循环持 [`Weak`]，无引用环）；
/// * control 循环：读 control stream，按语义 stream_id 路由 StreamOpened /
///   CloseStream / Error 到对应会话的事件通道。
pub struct QuicLink {
    conn: quinn::Connection,
    control_tx: Mutex<quinn::SendStream>,
    /// 流登记互斥：`open_bi + 就绪信号 + OpenStream` 串行——保证对端
    /// FIFO 配对顺序（accept_bi 与 OpenStream 一一对应，不串流）。
    open: Mutex<()>,
    /// quic stream id → 会话事件通道（control 循环路由目标）。
    sessions: Mutex<HashMap<u64, mpsc::UnboundedSender<LinkEvent>>>,
    /// 语义 stream_id → quic stream id（StreamOpened/CloseStream 反查）。
    semantic: Mutex<HashMap<String, u64>>,
    stats: SharedStats,
}

impl QuicLink {
    /// 建立到 `addr` 的 QUIC 连接（客户端约定：先开 control stream 并发送
    /// 0 长度就绪信号，再进入 control 循环）。
    async fn connect(addr: SocketAddr) -> Result<Arc<Self>, TransportError> {
        let endpoint = quinn::Endpoint::client("0.0.0.0:0".parse().expect("静态地址"))
            .map_err(|e| TransportError::Io(e.to_string()))?;
        let connecting = endpoint
            .connect_with(client_config()?, addr, "stross")
            .map_err(|e| TransportError::Connect(format!("QUIC 连接失败: {e}")))?;
        let connection = connecting
            .await
            .map_err(|e| TransportError::Connect(format!("QUIC 握手失败: {e}")))?;
        let (mut control_tx, control_rx) = connection
            .open_bi()
            .await
            .map_err(|e| TransportError::Connect(format!("QUIC 开 control stream 失败: {e}")))?;
        control_tx
            .write_all(&0u32.to_le_bytes())
            .await
            .map_err(|e| TransportError::Connect(format!("QUIC control 就绪信号失败: {e}")))?;
        tracing::info!("QUIC 已连接（共享链路）: {addr}");
        let link = Arc::new(Self {
            conn: connection,
            control_tx: Mutex::new(control_tx),
            open: Mutex::new(()),
            sessions: Mutex::new(HashMap::new()),
            semantic: Mutex::new(HashMap::new()),
            stats: Arc::new(TransportStats::default()),
        });
        spawn_control_loop(Arc::downgrade(&link), control_rx);
        Ok(link)
    }

    /// 新建一条媒体会话（懒开 bi stream：首个控制消息发送时登记；
    /// 登记与 OpenStream 串行保证对端 FIFO 配对）。
    async fn open_media(self: &Arc<Self>) -> QuicMediaSession {
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        QuicMediaSession {
            link: self.clone(),
            tx: Mutex::new(None),
            rx: Mutex::new(None),
            events: Mutex::new(ev_rx),
            semantic: Mutex::new(None),
            role: Mutex::new(None),
            registered: AtomicBool::new(false),
            opened: AtomicBool::new(false),
            _ev_tx: ev_tx,
        }
    }

    /// 发送控制消息（control stream，长度前缀 JSON）。
    async fn send_control(&self, msg: ControlMessage) -> Result<(), TransportError> {
        let text = msg.to_text();
        let mut tx = self.control_tx.lock().await;
        write_msg(&mut tx, text.as_bytes()).await?;
        self.stats.add_sent(LEN_BYTES + text.len());
        Ok(())
    }
}

/// 客户端控制循环：读 control stream，按语义 id 路由事件到媒体会话。
/// 持 [`Weak`]——链路 drop（连接关闭）后循环自然退出，无引用环。
fn spawn_control_loop(link: Weak<QuicLink>, mut rx: quinn::RecvStream) {
    tokio::spawn(async move {
        while let Ok(Some(bytes)) = read_msg(&mut rx).await {
                    let text = match std::str::from_utf8(&bytes) {
                        Ok(t) => t,
                        Err(_) => continue,
                    };
                    let msg = match ControlMessage::from_text(text) {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    let Some(link) = link.upgrade() else {
                        break;
                    };
                    let qid = match &msg {
                        ControlMessage::StreamOpened { stream_id }
                        | ControlMessage::CloseStream { stream_id } => {
                            link.semantic.lock().await.get(stream_id).copied()
                        }
                        _ => None,
                    };

                    match msg {
                        ControlMessage::StreamOpened { .. } => {
                            if let Some(qid) = qid
                                && let Some(tx) = link.sessions.lock().await.get(&qid)
                            {
                                let _ = tx.send(LinkEvent::Opened);
                            }
                        }
                        ControlMessage::CloseStream { .. } => {
                            if let Some(qid) = qid
                                && let Some(tx) = link.sessions.lock().await.get(&qid)
                            {
                                let _ = tx.send(LinkEvent::Closed);
                            }
                        }
                        ControlMessage::Error { message } => {
                            // 路由给全部未就绪会话（OpenStream 被拒等；
                            // 已就绪会话忽略——正常媒体流不产生 Error）
                            for tx in link.sessions.lock().await.values() {
                                let _ = tx.send(LinkEvent::Error(message.clone()));
                            }
                        }
                        _ => {}
                    }
        }
    });
}

/// 链路表键：对端地址（host, port）。
type LinkKey = (String, u16);

/// 进程级链路管理器：`(host, port)` → 共享连接（弱引用；全部会话关闭即回收）。
static QUIC_LINKS: OnceLock<Mutex<HashMap<LinkKey, Weak<QuicLink>>>> = OnceLock::new();

/// 获取/建立到 `addr` 的共享连接（get-or-create 串行，避免并发重复建连）。
async fn link_for(addr: SocketAddr) -> Result<Arc<QuicLink>, TransportError> {
    let key = (addr.ip().to_string(), addr.port());
    let map = QUIC_LINKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().await;
    if let Some(link) = guard.get(&key).and_then(Weak::upgrade) {
        return Ok(link);
    }
    let link = QuicLink::connect(addr).await?;
    guard.insert(key, Arc::downgrade(&link));
    Ok(link)
}

/// 绑定到共享连接上一条媒体流的会话（对上层呈现普通 [`DataSession`]）。
///
/// * 首个控制消息拦截：`Hello` → `OpenStream{role: Push}`、
///   `Watch` → `OpenStream{role: Watch}`（经链路 control stream 登记）；
/// * `Bye` / [`DataSession::close`] → `CloseStream` + 结束 bi stream；
/// * 媒体帧 → v2 紧凑帧头（[`Frame2`]）写 bi stream；
/// * `recv`：StreamOpened 事件 → `Welcome`（push）/ `Ready`（watch），
///   其余 → 紧凑帧解码为 v1 [`Frame`]（codec=0，接收侧不读）。
pub struct QuicMediaSession {
    link: Arc<QuicLink>,
    tx: Mutex<Option<quinn::SendStream>>,
    rx: Mutex<Option<quinn::RecvStream>>,
    events: Mutex<mpsc::UnboundedReceiver<LinkEvent>>,
    semantic: Mutex<Option<String>>,
    role: Mutex<Option<StreamRole>>,
    registered: AtomicBool,
    /// 确认门：StreamOpened（Welcome/Ready）已送达。未确认前不吐媒体帧——
    /// 先到帧留在 QUIC 缓冲（流控），避免上层等确认时误消费首帧。
    opened: AtomicBool,
    /// 持有发送端使链路登记到会话存在期间（Drop 时随会话释放）。
    _ev_tx: mpsc::UnboundedSender<LinkEvent>,
}

impl QuicMediaSession {
    /// 登记本会话（首个控制消息触发；`open_bi + 就绪信号 + OpenStream`
    /// 在链路 open 互斥下串行——保证对端 FIFO 配对不串流）。
    async fn register(&self, open: ControlMessage) -> Result<(), TransportError> {
        let _guard = self.link.open.lock().await;
        if self.registered.swap(true, Ordering::SeqCst) {
            return Ok(()); // 已登记（双 send 竞态防御）
        }
        let (stream_id, role, title, video, audio, share_token) = match open {
            ControlMessage::OpenStream {
                stream_id,
                role,
                title,
                video,
                audio,
                share_token,
            } => (stream_id, role, title, video, audio, share_token),
            _ => unreachable!("register 只收 OpenStream"),
        };
        // 懒开 bi stream（就绪信号让对端 accept_bi 返回）
        let (tx, rx) = self
            .link
            .conn
            .open_bi()
            .await
            .map_err(|e| TransportError::Io(format!("QUIC 开媒体 stream 失败: {e}")))?;
        let qid: u64 = tx.id().into();
        let mut tx = tx;
        tx.write_all(&0u32.to_le_bytes())
            .await
            .map_err(|e| TransportError::Io(format!("QUIC 媒体就绪信号失败: {e}")))?;
        *self.tx.lock().await = Some(tx);
        *self.rx.lock().await = Some(rx);
        *self.semantic.lock().await = Some(stream_id.clone());
        *self.role.lock().await = Some(role);
        self.link
            .sessions
            .lock()
            .await
            .insert(qid, self._ev_tx.clone());
        self.link
            .semantic
            .lock()
            .await
            .insert(stream_id.clone(), qid);
        self.link.send_control(ControlMessage::OpenStream {
            stream_id,
            role,
            title,
            video,
            audio,
            share_token,
        })
        .await
    }

    /// 确保已登记（首个媒体帧 / 控制消息之前）。
    async fn ensure_registered(&self) -> Result<(), TransportError> {
        if !self.registered.load(Ordering::SeqCst) {
            // 理论上不会发生（sender/watch 先发控制消息）；防御性错误
            return Err(TransportError::Protocol(
                "QUIC 媒体会话未登记（应先发 Hello/Watch）".into(),
            ));
        }
        Ok(())
    }

    /// 优雅结束媒体流：CloseStream（如已登记）+ 结束 bi stream。
    async fn finish(&self) -> Result<(), TransportError> {
        if let Some(sid) = self.semantic.lock().await.clone() {
            let _ = self
                .link
                .send_control(ControlMessage::CloseStream { stream_id: sid })
                .await;
        }
        if let Some(tx) = self.tx.lock().await.as_mut() {
            let _ = tx.finish();
        }
        Ok(())
    }

    /// 读一条媒体消息（跳过空就绪信号；`Ok(None)` = 流结束）。
    async fn recv_media(&self) -> Result<Option<Bytes>, TransportError> {
        let mut guard = self.rx.lock().await;
        let Some(rx) = guard.as_mut() else {
            return Err(TransportError::Protocol("QUIC 媒体会话未登记".into()));
        };
        loop {
            match read_msg(rx).await? {
                Some(b) if b.is_empty() => continue,
                other => return Ok(other),
            }
        }
    }
}

#[async_trait]
impl DataSession for QuicMediaSession {
    fn peer_addr(&self) -> Option<std::net::SocketAddr> {
        Some(self.link.conn.remote_address())
    }

    async fn send(&self, pkt: SessionPacket) -> Result<(), TransportError> {
        match pkt {
            SessionPacket::Control(ControlMessage::Hello {
                stream_id,
                title,
                video,
                audio,
                share_token,
            }) => {
                if !self.registered.load(Ordering::SeqCst) {
                    self.register(ControlMessage::OpenStream {
                        stream_id,
                        role: StreamRole::Push,
                        title: Some(title),
                        video,
                        audio,
                        share_token,
                    })
                    .await?;
                }
                Ok(())
            }
            SessionPacket::Control(ControlMessage::Watch { stream_id }) => {
                if !self.registered.load(Ordering::SeqCst) {
                    self.register(ControlMessage::OpenStream {
                        stream_id,
                        role: StreamRole::Watch,
                        title: None,
                        video: None,
                        audio: None,
                        share_token: None,
                    })
                    .await?;
                }
                Ok(())
            }
            SessionPacket::Control(ControlMessage::Bye) => self.finish().await,
            // 本链路不经媒体会话发其它控制（控制走链路的 control stream）
            SessionPacket::Control(_) => Ok(()),
            SessionPacket::Media(frame) => {
                self.ensure_registered().await?;
                let full = Frame2::from_frame(&frame).to_bytes();
                let mut tx = self.tx.lock().await;
                let Some(tx) = tx.as_mut() else {
                    return Err(TransportError::Protocol("QUIC 媒体会话未登记".into()));
                };
                write_msg(tx, &full).await?;
                self.link.stats.add_sent(LEN_BYTES + full.len());
                Ok(())
            }
        }
    }

    async fn recv(&self) -> Result<Option<SessionPacket>, TransportError> {
        // 确认门：StreamOpened（Welcome/Ready）未达前不吐媒体帧——先到帧
        // 留在 QUIC 缓冲（流控），避免上层（connect_watch 等）等确认时把
        // 首关键帧当杂包吞掉（负载下复现：观看端收不到首帧超时）。
        if !self.opened.load(Ordering::SeqCst) {
            let ev = self.events.lock().await.recv().await;
            return self.on_event(ev).await;
        }
        // 已确认：事件与媒体并取，**事件优先**（同旧实现 biased 控制优先：
        // 控制消息/关闭信号不被媒体洪流饿死）
        let mut events = self.events.lock().await;
        tokio::select! {
            biased;
            ev = events.recv() => self.on_event(ev).await,
            r = self.recv_media() => {
                let Some(bytes) = r? else { return Ok(None) };
                self.link.stats.add_recv(LEN_BYTES + bytes.len());
                let frame = Frame2::to_frame_owned(bytes)
                    .map_err(|e| TransportError::Protocol(e.to_string()))?;
                Ok(Some(SessionPacket::Media(frame)))
            }
        }
    }

    async fn close(&self) -> Result<(), TransportError> {
        self.finish().await
    }
}

impl QuicMediaSession {
    /// 处理一条链路事件（公共：ack 转换 / 错误 / 关闭）。
    async fn on_event(
        &self,
        ev: Option<LinkEvent>,
    ) -> Result<Option<SessionPacket>, TransportError> {
        match ev {
            Some(LinkEvent::Opened) => {
                self.opened.store(true, Ordering::SeqCst);
                let role = self.role.lock().await.unwrap_or(StreamRole::Push);
                let sid = self.semantic.lock().await.clone().unwrap_or_default();
                Ok(Some(SessionPacket::Control(match role {
                    StreamRole::Push => ControlMessage::Welcome { stream_id: sid },
                    StreamRole::Watch => ControlMessage::Ready { stream_id: sid },
                })))
            }
            Some(LinkEvent::Error(message)) => Err(TransportError::Protocol(message)),
            Some(LinkEvent::Closed) | None => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// 服务端：连接（QuicServerLink）——中继 peer 循环的传输原语
// ---------------------------------------------------------------------------

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

    /// 接受一个入站**连接**（Phase C：一条连接 = 一条链路，可开 N 媒体流；
    /// 中继侧 peer 循环消费 [`QuicServerLink`] 的 control / accept_media 原语）。
    pub async fn accept_link(&mut self) -> Result<QuicServerLink, TransportError> {
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
        tracing::info!("QUIC 入站连接（链路）: {peer}");
        Ok(QuicServerLink {
            inner: Arc::new(QuicServerLinkInner {
                conn: connection,
                control_tx: Mutex::new(control_tx),
                control_rx: Mutex::new(control_rx),
                stats: self.stats.clone(),
            }),
        })
    }
}

/// 服务端连接（链路）句柄：control 收发 + 媒体流 accept 原语。
pub struct QuicServerLink {
    inner: Arc<QuicServerLinkInner>,
}

/// 服务端连接（链路）内部状态（Arc 包裹：peer 循环与每条流的任务共享）。
struct QuicServerLinkInner {
    conn: quinn::Connection,
    control_tx: Mutex<quinn::SendStream>,
    control_rx: Mutex<quinn::RecvStream>,
    stats: SharedStats,
}

impl Clone for QuicServerLink {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl QuicServerLink {
    /// 对端地址（来源感知门控用）。
    pub fn peer_addr(&self) -> SocketAddr {
        self.inner.conn.remote_address()
    }

    /// quinn 连接（`closed()` 监测链路关闭）。
    pub fn conn(&self) -> &quinn::Connection {
        &self.inner.conn
    }

    /// 收一条控制消息（跳过空就绪信号）；`Ok(None)` = 连接关闭。
    pub async fn recv_control(&self) -> Result<Option<ControlMessage>, TransportError> {
        let mut guard = self.inner.control_rx.lock().await;
        loop {
            match read_msg(&mut guard).await? {
                Some(b) if b.is_empty() => continue,
                Some(b) => {
                    let text = std::str::from_utf8(&b)
                        .map_err(|e| TransportError::Protocol(e.to_string()))?;
                    let msg = ControlMessage::from_text(text)
                        .map_err(|e| TransportError::Protocol(e.to_string()))?;
                    return Ok(Some(msg));
                }
                None => return Ok(None),
            }
        }
    }

    /// 发送控制消息（StreamOpened / Error / CloseStream）。
    pub async fn send_control(&self, msg: ControlMessage) -> Result<(), TransportError> {
        let text = msg.to_text();
        let mut tx = self.inner.control_tx.lock().await;
        write_msg(&mut tx, text.as_bytes()).await?;
        self.inner.stats.add_sent(LEN_BYTES + text.len());
        Ok(())
    }

    /// 接受下一个媒体 bi stream（客户端就绪信号后返回）。
    /// 一次只允许一个待决 `accept_bi`（peer 循环 pinned future 复用）。
    pub async fn accept_media(
        &self,
    ) -> Result<(quinn::SendStream, quinn::RecvStream, u64), TransportError> {
        let (tx, rx) = self
            .inner
            .conn
            .accept_bi()
            .await
            .map_err(|e| TransportError::Io(format!("QUIC accept_bi 失败: {e}")))?;
        let id: u64 = tx.id().into();
        Ok((tx, rx, id))
    }
}

// ---------------------------------------------------------------------------
// 分帧助手（长度前缀；与 WS/旧 QUIC 同构）
// ---------------------------------------------------------------------------

/// 读一条媒体消息并解码为 v1 帧（v2 紧凑帧头；跳过空就绪信号；
/// `Ok(None)` = 流结束）。中继 QUIC 推流任务用。
pub async fn read_media_frame(rx: &mut quinn::RecvStream) -> Result<Option<Frame>, TransportError> {
    loop {
        match read_msg(rx).await? {
            Some(b) if b.is_empty() => continue,
            Some(b) => {
                let f = Frame2::to_frame_owned(b)
                    .map_err(|e| TransportError::Protocol(e.to_string()))?;
                return Ok(Some(f));
            }
            None => return Ok(None),
        }
    }
}

/// 写一条媒体帧（v2 紧凑帧头）。中继 QUIC 观看任务用。
pub async fn write_media_frame(
    tx: &mut quinn::SendStream,
    frame: &Frame,
) -> Result<(), TransportError> {
    let full = Frame2::from_frame(frame).to_bytes();
    write_msg(tx, &full).await
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

/// 单条 QUIC 消息长度上限：读侧长度由对端声明，无上限会按对端声明的
/// 4GiB 分配（局域网内任意能完成 TLS 握手的对端可打挂 Android 中继）。
/// 与 WS 的 64MB 限制对齐；写侧另有 4GiB 上限（长度前缀 u32 语义）。
const MAX_MSG_LEN: usize = 64 * 1024 * 1024;

/// 读一条长度前缀消息；`Ok(None)` = 流干净结束（对端关闭该 stream）。
async fn read_msg(rx: &mut quinn::RecvStream) -> Result<Option<Bytes>, TransportError> {
    let mut len_buf = [0u8; LEN_BYTES];
    if let Err(e) = rx.read_exact(&mut len_buf).await {
        return map_read_err(e).map(|_| None);
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_MSG_LEN {
        return Err(TransportError::Protocol(format!(
            "QUIC 消息长度超限（{len} > {MAX_MSG_LEN} 字节）"
        )));
    }
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

/// 服务端连接空闲超时：对端硬断连（force-stop / 拔网线）后，中继在该时长内
/// 收不到任何数据包即判定连接死亡，读循环返回并清理流（quinn 默认 30s 太慢，
/// 真机实测 force-stop 后流残留近半分钟）。传媒流帧连续，正常推流不触发；
/// 静默但存活的观看连接由客户端 keep-alive（10s < 15s）续命。
const SERVER_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// 客户端 keep-alive 间隔：保证静默观看连接不被服务端 idle 掐断，同时维持
/// NAT 映射（Android 观看端长期挂后台场景）。
const CLIENT_KEEP_ALIVE: std::time::Duration = std::time::Duration::from_secs(10);

/// 服务端 TLS/crypto 配置（不含传输参数；idle 超时由调用方设置）。
fn server_crypto_config() -> Result<quinn::ServerConfig, String> {
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
}

fn server_config() -> Result<quinn::ServerConfig, TransportError> {
    static CONFIG: OnceLock<Result<quinn::ServerConfig, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut config = server_crypto_config()?;
            let idle = SERVER_IDLE_TIMEOUT
                .try_into()
                .map_err(|e| format!("idle 超时配置失败: {e}"))?;
            let mut transport = quinn::TransportConfig::default();
            transport.max_idle_timeout(Some(idle));
            config.transport_config(Arc::new(transport));
            Ok(config)
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
            let mut client = quinn::ClientConfig::new(Arc::new(quic));
            let idle = SERVER_IDLE_TIMEOUT
                .try_into()
                .map_err(|e| format!("idle 超时配置失败: {e}"))?;
            let mut transport = quinn::TransportConfig::default();
            transport.max_idle_timeout(Some(idle));
            transport.keep_alive_interval(Some(CLIENT_KEEP_ALIVE));
            client.transport_config(Arc::new(transport));
            Ok(client)
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
    use stross_proto::frame::{CODEC_H264, FLAG_KEYFRAME, Frame, MAGIC, TRACK_VIDEO};

    /// 服务端测试用 peer 配对循环：control OpenStream ↔ accept_bi FIFO 配对，
    /// 与中继 [`crate::relay::data_plane::quic_peer_loop`] 同构的最小版。
    /// 每条配对后回 `StreamOpened`（客户端 StreamOpened→Welcome/Ready 转换依赖）。
    /// 返回 (stream_id, SendStream, RecvStream)——调用方持 SendStream 收对端帧。
    async fn server_pair_loop(
        link: QuicServerLink,
        stream_count: usize,
    ) -> Vec<(String, quinn::SendStream, quinn::RecvStream)> {
        use std::collections::VecDeque;
        let mut opens: VecDeque<ControlMessage> = VecDeque::new();
        let mut streams: VecDeque<(quinn::SendStream, quinn::RecvStream)> = VecDeque::new();
        let mut accept = Box::pin(link.accept_media());
        let mut paired: Vec<(String, quinn::SendStream, quinn::RecvStream)> = Vec::new();
        while paired.len() < stream_count {
            tokio::select! {
                msg = link.recv_control() => {
                    if let Some(m) = msg.expect("control 读失败") {
                            opens.push_back(m);
                    }
                }
                res = &mut accept => {
                    if let Ok((tx, rx, _)) = res {
                        streams.push_back((tx, rx));
                        accept = Box::pin(link.accept_media());
                    }
                }
            }
            while !opens.is_empty() && !streams.is_empty() {
                let open = opens.pop_front().unwrap();
                let (tx, rx) = streams.pop_front().unwrap();
                let sid = match &open {
                    ControlMessage::OpenStream { stream_id, .. } => stream_id.clone(),
                    _ => unreachable!(),
                };
                link.send_control(ControlMessage::StreamOpened {
                    stream_id: sid.clone(),
                })
                .await
                .unwrap();
                paired.push((sid, tx, rx));
            }
        }
        paired
    }

    /// 真 UDP loopback 上的 QUIC **连接复用** roundtrip：
    /// 客户端一条共享连接开两条媒体流（push + watch 形态），服务端 peer 循环
    /// 按序配对——两路流独立收帧，互不串流（Phase C 核心语义）。
    #[tokio::test]
    async fn quic_link_multiplex_two_media_streams() {
        let transport = QuicTransport::new();
        let mut handle = transport
            .bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = handle.local_addr();
        let accept_task = tokio::spawn(async move {
            let link = handle.accept_link().await.unwrap();
            let paired = server_pair_loop(link, 2).await;
            (paired, handle)
        });

        // 客户端：同一 (host, port) 两条会话共享一条连接
        let peer = PeerAddr {
            transport: TransportId::Quic,
            addr: format!("quic://127.0.0.1:{}", addr.port()),
        };
        let params = SessionParams {
            session_id: "s1".into(),
            profile: ReliabilityProfile::Lossless,
        };
        let s1 = transport.connect(&peer, &params).await.unwrap();
        let s2 = transport.connect(&peer, &params).await.unwrap();
        assert!(s1.peer_addr().is_some());

        // push 形态（Hello → OpenStream push；Welcome 回执）
        s1.send(SessionPacket::Control(ControlMessage::Hello {
            stream_id: "stream-a".into(),
            title: "a".into(),
            video: None,
            audio: None,
            share_token: None,
        }))
        .await
        .unwrap();
        // watch 形态（Watch → OpenStream watch；Ready 回执）
        s2.send(SessionPacket::Control(ControlMessage::Watch {
            stream_id: "stream-b".into(),
        }))
        .await
        .unwrap();

        let (mut paired, _handle) = accept_task.await.unwrap();
        assert_eq!(paired.len(), 2, "两条媒体流按序配对");
        assert_eq!(paired[0].0, "stream-a");
        assert_eq!(paired[1].0, "stream-b");

        // 每条流独立发帧（紧凑帧头 v2 线上格式；s1 → stream-a、s2 → stream-b）
        let frames = [
            Frame::new(TRACK_VIDEO, CODEC_H264, FLAG_KEYFRAME, 0, vec![0xA0; 16]),
            Frame::new(TRACK_VIDEO, CODEC_H264, FLAG_KEYFRAME, 1, vec![0xA1; 16]),
        ];
        s1.send(SessionPacket::Media(frames[0].clone())).await.unwrap();
        s2.send(SessionPacket::Media(frames[1].clone())).await.unwrap();
        // 服务端收到两帧：内容逐字节一致（紧凑头解码 → v1 Frame；跳过空就绪信号）
        for (i, (_sid, _tx, rx)) in paired.iter_mut().enumerate() {
            let mut rx = rx;
            let bytes = loop {
                match read_msg(&mut rx).await.unwrap() {
                    Some(b) if b.is_empty() => continue,
                    other => break other.expect("应收到帧消息"),
                }
            };
            let frame = Frame2::to_frame(&bytes).expect("紧凑帧解码");
            assert_eq!(frame.header.track, TRACK_VIDEO);
            assert_eq!(frame.header.pts_ms, i as u32);
            assert_eq!(frame.payload.to_vec(), vec![0xA0 + i as u8; 16]);
        }

        // 客户端收 Welcome/Ready 回执（StreamOpened 事件转换）
        let ack1 = s1.recv().await.unwrap().unwrap();
        assert!(matches!(
            ack1,
            SessionPacket::Control(ControlMessage::Welcome { .. })
        ));
        let ack2 = s2.recv().await.unwrap().unwrap();
        assert!(matches!(
            ack2,
            SessionPacket::Control(ControlMessage::Ready { .. })
        ));

        // 优雅关闭：CloseStream + 结束流
        s1.close().await.unwrap();
        s2.close().await.unwrap();
    }

    #[test]
    fn profile_is_lossless() {
        assert_eq!(QuicTransport::new().profile(), ReliabilityProfile::Lossless);
        assert_eq!(QuicTransport::new().id(), TransportId::Quic);
    }

    /// 测试用服务端配置：可指定 idle 超时（生产走进程级静态 15s；
    /// 这里用 2s 让「硬断连检测」测试秒级完成）。
    fn server_config_with_idle(
        idle: std::time::Duration,
    ) -> Result<quinn::ServerConfig, TransportError> {
        let mut config = server_crypto_config().map_err(TransportError::Protocol)?;
        let idle = idle
            .try_into()
            .map_err(|e| TransportError::Protocol(format!("idle 配置失败: {e}")))?;
        let mut t = quinn::TransportConfig::default();
        t.max_idle_timeout(Some(idle));
        config.transport_config(Arc::new(t));
        Ok(config)
    }

    /// 硬断连（对端被 force-stop，无任何再见包）检测：服务端在 idle 超时内
    /// 判死连接，`recv_control` 返回（中继 peer 循环据此清理）。
    #[tokio::test]
    async fn hard_disconnect_released_by_idle_timeout() {
        let endpoint = quinn::Endpoint::server(
            server_config_with_idle(std::time::Duration::from_secs(2)).unwrap(),
            "127.0.0.1:0".parse().unwrap(),
        )
        .unwrap();
        let server_addr = endpoint.local_addr().unwrap();
        // 服务端：接受连接 + control stream（不读，等 idle 判死）
        let server_task = tokio::spawn(async move {
            let conn = endpoint
                .accept()
                .await
                .unwrap()
                .accept()
                .unwrap()
                .await
                .unwrap();
            let (ctx, crx) = conn.accept_bi().await.unwrap();
            (conn, ctx, crx)
        });

        // 客户端：裸 quinn 建立连接（与 link_for 相同握手），然后整体 drop
        // （无 close 帧，等同 force-stop / 拔线）
        let client_ep = quinn::Endpoint::client("0.0.0.0:0".parse().unwrap()).unwrap();
        let conn = client_ep
            .connect_with(client_config().unwrap(), server_addr, "stross")
            .unwrap()
            .await
            .unwrap();
        let (mut ctx, mut crx) = conn.open_bi().await.unwrap();
        ctx.write_all(&0u32.to_le_bytes()).await.unwrap();
        let (_conn, _ctx, scrx) = server_task.await.unwrap();
        let _ = crx;
        drop(conn);
        drop(ctx);

        // 服务端 control 读：先消费客户端就绪信号（空消息），
        // 再等 idle 判死 → 读错误 / 关闭返回（触发清理）
        let mut scrx = scrx;
        let ready = read_msg(&mut scrx).await.expect("control 读应成功");
        assert!(
            ready.as_ref().is_some_and(|b| b.is_empty()),
            "首条应为就绪空消息: {ready:?}"
        );
        let r = tokio::time::timeout(std::time::Duration::from_secs(6), read_msg(&mut scrx)).await;
        match r {
            Ok(Ok(None)) | Ok(Err(_)) => {} // 干净结束或判死错误都触发清理
            other => panic!("硬断连后 control 读应在 idle 内返回，得到 {other:?}"),
        }
    }

    /// 紧凑帧头 v2 的「magic 不再需要」：Frame2 头与旧 v1 头在线上互斥
    /// （仅 QUIC 复用连接用 Frame2；WS/SRT 单流路径保留 v1，见 frame.rs 测试）。
    #[test]
    fn compact_header_has_no_magic() {
        let f = Frame::new(TRACK_VIDEO, CODEC_H264, FLAG_KEYFRAME, 0, vec![1u8; 8]);
        let compact = Frame2::from_frame(&f).to_bytes();
        assert!(
            !compact.starts_with(MAGIC),
            "紧凑头不带 v1 魔数（长度前缀 + 协商声明提供上下文）"
        );
    }
}
