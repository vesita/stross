//! WebRTC 传输实现（str0m 0.23，Sans-IO；设计文档 docs/framework-v3.md §4）。
//!
//! 中继侧 WebRTC peer：一个 UDP socket + 一个 [`str0m::Rtc`] 实例，两个 data channel：
//!
//! * `"control"`：可靠、有序 —— 控制消息（JSON 文本帧）
//! * `"media"`：不可靠、乱序（`MaxRetransmits { retransmits: 0 }`）—— 媒体帧
//!   （24 字节 v2 帧头 + 载荷，与 WS 线格式一致）
//!
//! 信令走 HTTP（relay 的 `POST /api/webrtc/start` + `/api/webrtc/answer`，标准 SDP 文本）；
//! 数据面与 WS 完全一致——同一 [`SessionPacket`] / [`DataSession`] 抽象，
//! relay 的 `handle_watch` 转发逻辑原样复用（这是传输抽象价值的证明）。
//!
//! 浏览器互操作注意：Chrome 在 http 源上会把 host 候选混淆为 mDNS `.local` 名，
//! 需要在解析 answer 前把候选行里的 `.local` 名解析成真实 IP
//! （[`resolve_mdns_candidates`]，feature `discovery` 开启时生效）。

use async_trait::async_trait;
use bytes::Bytes;
#[cfg(feature = "discovery")]
use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use str0m::change::{SdpAnswer, SdpPendingOffer};
use str0m::channel::{ChannelConfig, ChannelId, Reliability};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc};
use tokio::net::UdpSocket;
use tokio::sync::{Mutex, mpsc, watch};

use stross_proto::message::{ReliabilityProfile, StreamId, TransportId};

use super::{
    DataSession, PeerAddr, SessionPacket, SessionParams, SharedStats, Transport, TransportError,
    TransportStats,
};

/// 控制通道标签（可靠、有序）。
pub const CHANNEL_CONTROL: &str = "control";
/// 媒体通道标签（不可靠、乱序）。
pub const CHANNEL_MEDIA: &str = "media";

/// WebRTC 传输（Lossy profile）。
pub struct WebRtcTransport {
    stats: SharedStats,
}

impl Default for WebRtcTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl WebRtcTransport {
    pub fn new() -> Self {
        Self {
            stats: Arc::new(TransportStats::default()),
        }
    }

    /// 中继侧：为一个观看端建立 WebRTC peer（control + media 双通道）。
    ///
    /// 返回（标准 SDP offer 文本, peer 句柄）。`bind` 为 UDP socket 绑定地址
    /// （如 `0.0.0.0:0` 让系统分配并监听所有接口）。
    pub async fn start_peer(
        &self,
        session_id: &StreamId,
        bind: SocketAddr,
    ) -> Result<(String, WebRtcPeer), TransportError> {
        let udp = Arc::new(
            UdpSocket::bind(bind)
                .await
                .map_err(|e| TransportError::Io(e.to_string()))?,
        );
        let port = udp
            .local_addr()
            .map_err(|e| TransportError::Io(e.to_string()))?
            .port();

        let mut rtc = Rtc::new(Instant::now());
        // 多候选：本机所有非回环 IP + 回环。socket 绑 0.0.0.0，出站源 IP 由
        // 路由决定，必然与其中一条候选一致，ICE 校验可通过（回环↔回环、LAN↔LAN）。
        let mut ips = crate::net::local_ips();
        ips.push(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        for ip in ips {
            match Candidate::host(SocketAddr::new(ip, port), "udp") {
                Ok(c) => {
                    rtc.add_local_candidate(c);
                }
                Err(e) => tracing::warn!("webrtc 候选构造失败 {ip}: {e}"),
            }
        }

        let mut api = rtc.sdp_api();
        let control_id = api.add_channel_with_config(ChannelConfig {
            label: CHANNEL_CONTROL.into(),
            ordered: true,
            reliability: Reliability::Reliable,
            negotiated: None,
            protocol: String::new(),
        });
        let media_id = api.add_channel_with_config(ChannelConfig {
            label: CHANNEL_MEDIA.into(),
            ordered: false,
            reliability: Reliability::MaxRetransmits { retransmits: 0 },
            negotiated: None,
            protocol: String::new(),
        });
        let Some((offer, pending)) = api.apply() else {
            return Err(TransportError::Protocol("无法生成 SDP offer".into()));
        };
        // str0m 生成的 SDP 自带 a=candidate（含 ufrag 扩展），直接回给观看端
        let sdp = offer.to_sdp_string();

        let (cmd_tx, cmd_rx) = mpsc::channel::<PeerCommand>(256);
        let (open_tx, open_rx) = watch::channel(false);

        Ok((
            sdp,
            WebRtcPeer {
                session_id: session_id.clone(),
                udp,
                rtc: Some(rtc),
                pending: Some(pending),
                control_id,
                media_id,
                cmd_tx: Some(cmd_tx),
                cmd_rx: Some(cmd_rx),
                channels_open: open_rx,
                open_tx,
            },
        ))
    }
}

#[async_trait]
impl Transport for WebRtcTransport {
    fn id(&self) -> TransportId {
        TransportId::WebRtc
    }

    fn profile(&self) -> ReliabilityProfile {
        ReliabilityProfile::Lossy
    }

    async fn connect(
        &self,
        _peer: &PeerAddr,
        _params: &SessionParams,
    ) -> Result<Box<dyn DataSession>, TransportError> {
        Err(TransportError::NotSupported(
            "webrtc 会话经信令建立：POST /api/webrtc/start + /answer",
        ))
    }

    async fn accept(
        &self,
        _params: &SessionParams,
    ) -> Result<Box<dyn DataSession>, TransportError> {
        Err(TransportError::NotSupported(
            "webrtc 服务端请使用 WebRtcTransport::start_peer + WebRtcPeer::accept_answer",
        ))
    }

    fn stats(&self) -> TransportStats {
        self.stats.as_ref().clone()
    }
}

/// 中继侧待完成信令的 WebRTC peer（start 与 answer 之间存于 relay 状态）。
pub struct WebRtcPeer {
    session_id: StreamId,
    udp: Arc<UdpSocket>,
    rtc: Option<Rtc>,
    pending: Option<SdpPendingOffer>,
    control_id: ChannelId,
    media_id: ChannelId,
    cmd_tx: Option<mpsc::Sender<PeerCommand>>,
    cmd_rx: Option<mpsc::Receiver<PeerCommand>>,
    channels_open: watch::Receiver<bool>,
    open_tx: watch::Sender<bool>,
}

impl WebRtcPeer {
    /// 会话 id（观看端要看的流）。
    pub fn session_id(&self) -> &StreamId {
        &self.session_id
    }

    /// 处理观看端 answer（标准 SDP 文本，可能含 mDNS 候选）。
    ///
    /// 成功后启动 run loop 并返回数据会话与就绪信号：
    /// * `channels_open`：control/media 双通道都打开后变 `true`（relay 据此启动转发）
    /// * `close_tx`：强制关闭（看门狗超时等）；drop 后 run loop 在会话结束时退出
    pub async fn accept_answer(
        &mut self,
        sdp: &str,
    ) -> Result<
        (
            Box<dyn DataSession>,
            watch::Receiver<bool>,
            mpsc::Sender<PeerCommand>,
        ),
        TransportError,
    > {
        let sdp = resolve_mdns_candidates(sdp).await;
        let answer = SdpAnswer::from_sdp_string(&sdp)
            .map_err(|e| TransportError::Protocol(format!("SDP answer 解析失败: {e}")))?;
        let mut rtc = self
            .rtc
            .take()
            .ok_or(TransportError::Protocol("peer 已被使用".into()))?;
        let pending = self
            .pending
            .take()
            .ok_or(TransportError::Protocol("peer 已被使用".into()))?;
        rtc.sdp_api()
            .accept_answer(pending, answer)
            .map_err(|e| TransportError::Protocol(format!("接受 answer 失败: {e}")))?;

        let cmd_rx = self
            .cmd_rx
            .take()
            .ok_or(TransportError::Protocol("peer 已被使用".into()))?;
        let close_tx = self
            .cmd_tx
            .take()
            .ok_or(TransportError::Protocol("peer 已被使用".into()))?;

        let (inbound_tx, inbound_rx) = mpsc::channel::<SessionPacket>(64);
        let stats: SharedStats = Arc::new(TransportStats::default());
        let udp = self.udp.clone();
        let control_id = self.control_id;
        let media_id = self.media_id;
        let open_tx = self.open_tx.clone();
        tokio::spawn(
            PeerLoop {
                udp,
                rtc,
                cmd_rx,
                inbound_tx,
                control_id,
                media_id,
                stats,
                open_tx,
            }
            .run(),
        );

        let session: Box<dyn DataSession> = Box::new(WebRtcDataSession {
            cmd: close_tx.clone(),
            inbound: Mutex::new(inbound_rx),
        });
        Ok((session, self.channels_open.clone(), close_tx))
    }
}

/// 运行循环命令（应用 → run loop；`close_tx` 供 relay 强制关闭）。
pub enum PeerCommand {
    Send(SessionPacket),
    Close,
}

/// WebRTC 数据会话（应用侧视图）。
struct WebRtcDataSession {
    cmd: mpsc::Sender<PeerCommand>,
    /// run loop → 应用 的入站包（`recv(&self)` 需 `&mut`，故包 Mutex）。
    inbound: Mutex<mpsc::Receiver<SessionPacket>>,
}

#[async_trait]
impl DataSession for WebRtcDataSession {
    async fn send(&self, pkt: SessionPacket) -> Result<(), TransportError> {
        self.cmd
            .send(PeerCommand::Send(pkt))
            .await
            .map_err(|_| TransportError::Closed)
    }

    async fn recv(&self) -> Result<Option<SessionPacket>, TransportError> {
        let mut rx = self.inbound.lock().await;
        Ok(rx.recv().await)
    }

    async fn close(&self) -> Result<(), TransportError> {
        let _ = self.cmd.send(PeerCommand::Close).await;
        Ok(())
    }
}

/// run loop 的上下文（参数较多，收进结构体避免 clippy::too_many_arguments）。
struct PeerLoop {
    udp: Arc<UdpSocket>,
    rtc: Rtc,
    cmd_rx: mpsc::Receiver<PeerCommand>,
    inbound_tx: mpsc::Sender<SessionPacket>,
    control_id: ChannelId,
    media_id: ChannelId,
    stats: SharedStats,
    open_tx: watch::Sender<bool>,
}

impl PeerLoop {
    /// run loop：UDP ↔ str0m 泵送 + 通道分发。
    ///
    /// 终止条件：`Event::Closed`、命令通道关闭、UDP/协议错误。
    async fn run(mut self) {
        let mut buf = vec![0u8; 64 * 1024];
        let mut next_timeout: Option<Instant> = None;
        let mut control_open = false;
        let mut media_open = false;
        let mut opened_sent = false;

        loop {
            // 1) 排空命令
            loop {
                match self.cmd_rx.try_recv() {
                    Ok(PeerCommand::Send(pkt)) => {
                        let (cid, binary, bytes) = match &pkt {
                            SessionPacket::Control(c) => (
                                self.control_id,
                                false,
                                Bytes::from(c.to_text().into_bytes()),
                            ),
                            SessionPacket::Media(f) => (self.media_id, true, f.to_bytes()),
                        };
                        // 注意：先写完再更新统计，避免跨 await 持有 Rtc 借用
                        let sent = if let Some(mut ch) = self.rtc.channel(cid) {
                            match ch.write(binary, &bytes) {
                                Ok(_) => Some(bytes.len()),
                                Err(e) => {
                                    tracing::warn!("webrtc 通道写失败: {e}");
                                    None
                                }
                            }
                        } else {
                            tracing::debug!("webrtc 通道未打开，丢弃 {} 字节", bytes.len());
                            None
                        };
                        if let Some(n) = sent {
                            self.stats.add_sent(n);
                        }
                    }
                    Ok(PeerCommand::Close) => {
                        let _ = self.rtc.close();
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        let _ = self.rtc.close();
                    }
                }
            }

            // 2) 等待 UDP 数据或超时
            let wait = next_timeout.map_or(Duration::from_secs(1), |t| {
                t.saturating_duration_since(Instant::now())
            });
            tokio::select! {
                res = self.udp.recv_from(&mut buf) => {
                    match res {
                        Ok((n, from)) => {
                            let local = self.udp.local_addr().unwrap_or(from);
                            if let Ok(recv) = Receive::new(Protocol::Udp, from, local, &buf[..n])
                                && let Err(e) = self.rtc.handle_input(Input::Receive(Instant::now(), recv))
                            {
                                tracing::warn!("webrtc handle_input: {e}");
                            }
                        }
                        Err(e) => {
                            tracing::warn!("webrtc udp recv: {e}");
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep(wait) => {
                    let _ = self.rtc.handle_input(Input::Timeout(Instant::now()));
                }
            }

            // 3) 排空输出
            loop {
                match self.rtc.poll_output() {
                    Ok(Output::Transmit(t)) => {
                        let n = t.contents.len();
                        if let Err(e) = self.udp.send_to(&t.contents[..], t.destination).await {
                            tracing::warn!("webrtc udp send_to: {e}");
                        }
                        self.stats.add_sent(n);
                    }
                    Ok(Output::Timeout(t)) => {
                        next_timeout = Some(t);
                        break;
                    }
                    Ok(Output::Event(ev)) => match ev {
                        Event::Connected => tracing::info!("webrtc 已连接（DTLS/ICE 就绪）"),
                        Event::IceConnectionStateChange(s) => {
                            tracing::debug!("webrtc ICE 状态: {s:?}");
                        }
                        Event::ChannelOpen(id, label) => {
                            tracing::info!("webrtc 通道打开: {label}");
                            if id == self.control_id {
                                control_open = true;
                            }
                            if id == self.media_id {
                                media_open = true;
                            }
                            if !opened_sent && control_open && media_open {
                                opened_sent = true;
                                let _ = self.open_tx.send(true);
                            }
                        }
                        Event::ChannelData(d) => {
                            let n = d.data.len();
                            // 先处理 media（move 走 d.data，避免借用冲突），再处理 control
                            let pkt = if d.id == self.media_id {
                                // Vec<u8> → Bytes（0 拷贝转移）→ 零拷贝切片载荷
                                stross_proto::frame::Frame::from_bytes_owned(d.data.into())
                                    .ok()
                                    .map(SessionPacket::Media)
                            } else if d.id == self.control_id {
                                String::from_utf8(d.data)
                                    .ok()
                                    .and_then(|s| {
                                        stross_proto::message::ControlMessage::from_text(&s).ok()
                                    })
                                    .map(SessionPacket::Control)
                            } else {
                                None
                            };
                            if let Some(pkt) = pkt {
                                self.stats.add_recv(n);
                                let _ = self.inbound_tx.send(pkt).await;
                            }
                        }
                        Event::ChannelClose(id) => {
                            tracing::debug!("webrtc 通道关闭: {id:?}");
                        }
                        Event::Closed => {
                            tracing::info!("webrtc 会话关闭");
                            return;
                        }
                        _ => {}
                    },
                    Err(e) => {
                        tracing::warn!("webrtc poll_output: {e}");
                        break;
                    }
                }
            }
        }
        tracing::debug!("webrtc run loop 退出");
    }
}

// ---------------------------------------------------------------------------
// SDP 候选辅助
// ---------------------------------------------------------------------------

/// 把 SDP 文本里 `a=candidate:` 行中的 `.local` 主机名解析为真实 IP。
///
/// Chrome 在 http 源上会把 host 候选混淆为 mDNS 名（RFC 8832）；
/// 必须解析成 IP 才能交给 ICE。feature `discovery` 关闭时原样返回
/// （Rust 对端通常用明文 IP，无需解析）。
async fn resolve_mdns_candidates(sdp: &str) -> String {
    if !sdp.contains(".local") {
        return sdp.to_string();
    }
    #[cfg(feature = "discovery")]
    {
        use std::sync::OnceLock;

        static DAEMON: OnceLock<mdns::ServiceDaemon> = OnceLock::new();
        let daemon =
            DAEMON.get_or_init(|| mdns::ServiceDaemon::new().expect("创建 mDNS daemon 失败"));

        let mut out = String::with_capacity(sdp.len() + 64);
        for line in sdp.lines() {
            let Some(host) = mdns_host_in_candidate(line) else {
                out.push_str(line);
                out.push('\n');
                continue;
            };
            // 解析 .local 名（超时 3s），拿第一个 IPv4/IPv6 地址
            let ip: Option<IpAddr> = match daemon.resolve_hostname(host, Some(3000)) {
                Ok(rx) => {
                    let mut found = None;
                    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
                    while tokio::time::Instant::now() < deadline {
                        match rx.recv_async().await {
                            Ok(mdns::HostnameResolutionEvent::AddressesFound(_, addrs)) => {
                                found = pick_first(&addrs);
                                break;
                            }
                            Ok(mdns::HostnameResolutionEvent::SearchTimeout(_))
                            | Ok(mdns::HostnameResolutionEvent::SearchStopped(_)) => break,
                            _ => {}
                        }
                    }
                    found
                }
                Err(e) => {
                    tracing::warn!("mDNS 解析失败: {e}");
                    None
                }
            };
            let rewritten = if let Some(ip) = ip {
                let mut parts: Vec<String> = line
                    .split(' ')
                    .map(std::string::ToString::to_string)
                    .collect();
                // a=candidate:foundation component protocol priority IP port ...
                // "a=candidate:1" "1" "udp" "2122260223" "<host>" "<port>" ...
                if parts.len() > 5 {
                    parts[4] = ip.to_string();
                }
                parts.join(" ")
            } else {
                tracing::warn!("mDNS 候选解析失败: {host}，保留原行");
                line.to_string()
            };
            out.push_str(&rewritten);
            out.push('\n');
        }
        out
    }
    #[cfg(not(feature = "discovery"))]
    {
        sdp.to_string()
    }
}

#[cfg(feature = "discovery")]
fn pick_first(addrs: &HashSet<mdns::ScopedIp>) -> Option<std::net::IpAddr> {
    // 优先 IPv4（局域网常见）；mdns-sd 0.21 地址带接口信息（ScopedIp）
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| addrs.iter().next())
        .map(mdns::ScopedIp::to_ip_addr)
}

/// 若 `a=candidate:` 行第 5 个 token 是 `.local` 名则返回它，否则 None。
#[cfg(any(feature = "discovery", test))]
fn mdns_host_in_candidate(line: &str) -> Option<&str> {
    if !line.starts_with("a=candidate:") {
        return None;
    }
    let tokens: Vec<&str> = line.split(' ').collect();
    // a=candidate:foundation component protocol priority IP port type ...
    let host = *tokens.get(4)?;
    if host.ends_with(".local") {
        Some(host)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mdns_host_detection() {
        assert_eq!(
            mdns_host_in_candidate("a=candidate:1 1 udp 2122260223 abc123.local 53522 typ host"),
            Some("abc123.local")
        );
        assert_eq!(
            mdns_host_in_candidate("a=candidate:1 1 udp 2122260223 192.168.1.5 53522 typ host"),
            None
        );
        assert_eq!(mdns_host_in_candidate("a=ice-ufrag:xyz"), None);
    }
}
