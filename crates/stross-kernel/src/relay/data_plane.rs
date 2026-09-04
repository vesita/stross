//! 数据面转发（传输无关；HTTP / SRT / QUIC 共用）。
//!
//! 本模块是中继的数据面热路径：把入站 [`DataSession`] 会话接入流表
//! （推流 / 观看 / 级联代理转发），只依赖
//! [`Transport`](crate::transport::Transport) 抽象，不感知具体传输；
//! 共享状态（流表 / 授权 / 事件）统一存于 [`RelayState`]（[`super`]）。
//!
//! 接入点：
//!
//! * [`handle_push`] / [`handle_watch`]：WebSocket 升级处直接调用（[`super::api`]）
//! * [`handle_connect`]：SRT / QUIC 监听统一入口，按首条控制消息分流
//!   （`Hello` = 推流，`Watch` = 观看）
//! * [`spawn_accept_loop`]：SRT / QUIC 通用入站 accept 循环（消除复制）
//! * [`proxy_uplink`]：级联代理拉取任务（上游中继 → 本地代理流）
//!
//! 观看端接入时机：视频只在关键帧（IDR）后开始转发（ffmpeg 已在关键帧前重复
//! SPS/PPS，因此等待关键帧即可）；音频 ADTS 自带配置，可直接转发。

use std::time::Duration;

use stross_proto::frame::TRACK_VIDEO;
use stross_proto::message::{ControlMessage, StreamId, StreamInfo};
use tokio::sync::broadcast;

use crate::transport::{DataSession, SessionPacket};

use super::{RelayEvent, RelayState, StreamEntry};

/// 推流端/代理上游静默超时：超过该时长未收到**任何**消息（媒体帧或控制），
/// 判定对端已死亡（进程被 kill、网络黑洞、客户端僵尸化）并拆除流。
///
/// 传输层（rsrt 的 peer-idle、quinn 的 idle timeout）在应用层事件缺席时
/// 不可靠（rsrt 对 SIGKILL 的 UDP 对端可能永远不触发），因此在数据面兜底：
/// 无媒体即无价值，静默超时直接删流，观看端经广播 channel 关闭自愈。
const PUSH_SILENCE_TIMEOUT: Duration = Duration::from_secs(10);

/// 观看端：等待 Ready，然后按关键帧对齐转发。
pub(super) async fn handle_watch(
    session: Box<dyn DataSession>,
    stream_id: StreamId,
    state: RelayState,
) {
    let Some(entry) = state.get(&stream_id) else {
        let _ = session
            .send(SessionPacket::Control(ControlMessage::Error {
                message: format!("流 {stream_id} 不存在"),
            }))
            .await;
        return;
    };
    let info = entry.info.clone();
    let mut rx = entry.tx.subscribe();
    let last_kf = entry.last_keyframe.clone();
    let watchers = entry.tx.receiver_count() as u32;
    // 尽早释放 entry（含 broadcast::Sender 的 clone）：流被删后原始 sender drop，
    // 广播 channel 关闭 → rx.recv() 返回 Closed → 本循环退出并关会话；
    // 否则 sender clone 让 channel 永活，观看会话（含级联代理）悬挂不清理。
    drop(entry);
    state.emit(RelayEvent::WatchersChanged {
        stream_id: stream_id.clone(),
        watchers,
    });
    let _ = session
        .send(SessionPacket::Control(ControlMessage::Ready {
            stream_id: stream_id.clone(),
        }))
        .await;

    let mut video_started = false;
    // 新观看者先收到最近一个关键帧（含 SPS/PPS），立刻可解码
    if let Some(kf) = last_kf {
        if session.send(SessionPacket::Media(kf)).await.is_err() {
            // 会话已死：补发观看数变化，避免计数泄漏
            drop(rx);
            state.emit(RelayEvent::WatchersChanged {
                stream_id: stream_id.clone(),
                watchers: 0,
            });
            return;
        }
        video_started = true;
    }

    loop {
        let frame = match rx.recv().await {
            Ok(f) => f,
            Err(broadcast::error::RecvError::Lagged(_)) => {
                // 掉帧：重新等下一个关键帧，避免从 GOP 中间开始
                video_started = false;
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        };
        if frame.header.track == TRACK_VIDEO
            && !video_started
            && !frame.header.is_keyframe()
            && !frame.header.is_config()
        {
            continue;
        }
        if frame.header.track == TRACK_VIDEO
            && (frame.header.is_keyframe() || frame.header.is_config())
        {
            video_started = true;
        }
        if session.send(SessionPacket::Media(frame)).await.is_err() {
            break;
        }
    }
    // 干净关闭会话（WS 发 Close 帧；WebRTC 触发 run loop 退出）
    let _ = session.close().await;
    // 订阅端已断开，广播新的观看者数量（流仍存在时取剩余 receiver 数）
    drop(rx);
    let watchers = state
        .get(&stream_id)
        .map_or(0, |e| e.tx.receiver_count() as u32);
    state.emit(RelayEvent::WatchersChanged {
        stream_id: stream_id.clone(),
        watchers,
    });
    tracing::debug!("观看端断开: {stream_id} ({})", info.title);
}

/// 推流端：Hello → 建流 → 转发帧；Bye / 断开 → 删流。
pub(super) async fn handle_push(session: Box<dyn DataSession>, state: RelayState) {
    handle_push_loop(session, state, None).await;
}

/// 入站监听器抽象：`accept` 一个已建立的数据会话（SRT / QUIC 监听句柄各自实现）。
#[async_trait::async_trait]
pub(super) trait AcceptLoop: Send {
    async fn accept_session(
        &mut self,
    ) -> Result<Box<dyn DataSession>, crate::transport::TransportError>;
}

#[async_trait::async_trait]
impl AcceptLoop for crate::transport::srt::SrtListenerHandle {
    async fn accept_session(
        &mut self,
    ) -> Result<Box<dyn DataSession>, crate::transport::TransportError> {
        self.accept().await
    }
}

/// 通用入站 accept 循环：接收入站连接交给 [`handle_connect`]；
/// 监听错误或停机信号退出。SRT 用（QUIC 走 [`spawn_quic_accept_loop`]——
/// 通信模式 v2 Phase C 连接复用，见下方）。
pub(super) fn spawn_accept_loop<L: AcceptLoop + 'static>(
    mut listener: L,
    state: RelayState,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    label: &'static str,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                res = listener.accept_session() => match res {
                    Ok(session) => {
                        let state = state.clone();
                        tokio::spawn(async move { handle_connect(session, state).await });
                    }
                    Err(e) => {
                        tracing::warn!("{label} accept 失败: {e}");
                        break;
                    }
                },
                _ = shutdown.changed() => break,
            }
        }
        tracing::debug!("{label} 监听已停止");
    })
}

// ---------------------------------------------------------------------------
// QUIC 连接复用（通信模式 v2 Phase C，docs/framework-v3.md §5）：
// 一条 QUIC 连接 = 一条节点间链路，承载 N 条媒体流。
// 链路级 peer 循环把 control OpenStream ↔ accept_bi 媒体流 FIFO 配对，
// 维护 [quic_stream_id → (语义 stream_id, 方向)] demux 表；每条流独立
// push/watch 任务，停一条不级联其它流。
// ---------------------------------------------------------------------------

/// QUIC 专用入站 accept 循环：每条连接进入 [`quic_peer_loop`]。
pub(super) fn spawn_quic_accept_loop(
    mut listener: crate::transport::quic::QuicListenerHandle,
    state: RelayState,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                res = listener.accept_link() => match res {
                    Ok(link) => {
                        let state = state.clone();
                        tokio::spawn(async move { quic_peer_loop(link, state).await });
                    }
                    Err(e) => {
                        tracing::warn!("QUIC accept 失败: {e}");
                        break;
                    }
                },
                _ = shutdown.changed() => break,
            }
        }
        tracing::debug!("QUIC 监听已停止");
    })
}

/// OpenStream 推流负载（`quic_push_stream` 参数分组，避免超长参数表）。
struct OpenPush {
    stream_id: StreamId,
    title: Option<String>,
    video: Option<stross_proto::message::TrackInfo>,
    audio: Option<stross_proto::message::TrackInfo>,
    share_token: Option<String>,
}

/// QUIC 链路 peer 循环（中继侧）：control 消息 ↔ 媒体流 FIFO 配对 +
/// 链路级 demux 表。连接关闭后结束（推流流表条目由推流任务读 EOF 清理）。
async fn quic_peer_loop(link: crate::transport::quic::QuicServerLink, state: RelayState) {
    use std::collections::{HashMap, VecDeque};
    let mut pending_opens: VecDeque<stross_proto::message::ControlMessage> = VecDeque::new();
    let mut pending_streams: VecDeque<(quinn::SendStream, quinn::RecvStream, u64)> =
        VecDeque::new();
    // 链路级 demux：quic stream id → (语义 stream_id, 方向)
    let mut by_quic: HashMap<u64, (StreamId, stross_proto::message::StreamRole)> = HashMap::new();
    let mut by_semantic: HashMap<StreamId, u64> = HashMap::new();
    let mut accept = Box::pin(link.accept_media());
    loop {
        tokio::select! {
            msg = link.recv_control() => match msg {
                Ok(Some(m @ stross_proto::message::ControlMessage::OpenStream { .. })) => {
                    pending_opens.push_back(m);
                }
                Ok(Some(stross_proto::message::ControlMessage::CloseStream { stream_id })) => {
                    // 流级拆解：仅当本链路推流该语义 id 时拆除流表条目
                    // （watch 方向不拆流——流由推流端/其它链路拥有）
                    if let Some(qid) = by_semantic.remove(&stream_id)
                        && let Some((sid, stross_proto::message::StreamRole::Push)) =
                            by_quic.remove(&qid)
                        && state.remove(&sid)
                    {
                        state.emit(RelayEvent::StreamEnded { stream_id: sid });
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            },
            res = &mut accept => match res {
                Ok((tx, rx, qid)) => {
                    pending_streams.push_back((tx, rx, qid));
                    accept = Box::pin(link.accept_media());
                }
                Err(_) => break,
            },
            _ = link.conn().closed() => break,
        }
        // FIFO 配对（先判空再 pop——避免元组求值提前消费单侧队列）
        while !pending_opens.is_empty() && !pending_streams.is_empty() {
            let open = pending_opens.pop_front().unwrap();
            let (tx, rx, qid) = pending_streams.pop_front().unwrap();
            let (stream_id, role, title, video, audio, share_token) = match open {
                stross_proto::message::ControlMessage::OpenStream {
                    stream_id,
                    role,
                    title,
                    video,
                    audio,
                    share_token,
                } => (stream_id, role, title, video, audio, share_token),
                _ => unreachable!("pending_opens 只收 OpenStream"),
            };
            by_quic.insert(qid, (stream_id.clone(), role));
            by_semantic.insert(stream_id.clone(), qid);
            match role {
                stross_proto::message::StreamRole::Push => {
                    let state = state.clone();
                    let link = link.clone();
                    let open = OpenPush {
                        stream_id,
                        title,
                        video,
                        audio,
                        share_token,
                    };
                    tokio::spawn(async move { quic_push_stream(link, state, open, tx, rx).await });
                }
                stross_proto::message::StreamRole::Watch => {
                    let state = state.clone();
                    let link = link.clone();
                    tokio::spawn(
                        async move { quic_watch_stream(link, state, stream_id, tx, rx).await },
                    );
                }
            }
        }
    }
    tracing::debug!("QUIC 链路已关闭（连接断开）");
}

/// QUIC 推流任务（每条媒体流一个）：OpenStream 已配对，本任务负责
/// 受控接入门控 → 建流 → 帧转发（v2 紧凑帧头）→ 断开清理。
async fn quic_push_stream(
    link: crate::transport::quic::QuicServerLink,
    state: RelayState,
    open: OpenPush,
    mut _tx: quinn::SendStream,
    mut rx: quinn::RecvStream,
) {
    let OpenPush {
        stream_id,
        title,
        video,
        audio,
        share_token,
    } = open;
    // 受控模式接入门控（同 handle_push_loop：回环预授权 / 非回环凭证）
    if state.is_controlled() {
        let local = link.peer_addr().ip().is_loopback();
        if !state.is_allowed(&stream_id, share_token.as_deref(), local) {
            tracing::warn!(
                "推流被拒绝: 流 {stream_id} 来源 {} 未授权（QUIC 复用连接）",
                if local { "回环" } else { "非回环" }
            );
            let _ = link
                .send_control(stross_proto::message::ControlMessage::Error {
                    message: format!("流 {stream_id} 未授权，请先创建会话或出示有效接入凭证"),
                })
                .await;
            let _ = link
                .send_control(stross_proto::message::ControlMessage::CloseStream {
                    stream_id: stream_id.clone(),
                })
                .await;
            return;
        }
    }
    if state.get(&stream_id).is_some() {
        let _ = link
            .send_control(stross_proto::message::ControlMessage::Error {
                message: format!("流 {stream_id} 已存在"),
            })
            .await;
        let _ = link
            .send_control(stross_proto::message::ControlMessage::CloseStream { stream_id })
            .await;
        return;
    }
    let (bs, _rx) = broadcast::channel(128);
    let info = stross_proto::message::StreamInfo {
        stream_id: stream_id.clone(),
        title: title.unwrap_or_default(),
        video,
        audio,
        started_at: stross_proto::time::unix_secs(),
        watchers: 0,
    };
    state.insert(StreamEntry {
        info: info.clone(),
        tx: bs,
        last_keyframe: None,
    });
    state.emit(RelayEvent::StreamStarted {
        stream_id: stream_id.clone(),
        info: info.clone(),
    });
    tracing::info!("推流开始: {stream_id} ({})", info.title);
    let _ = link
        .send_control(stross_proto::message::ControlMessage::StreamOpened {
            stream_id: stream_id.clone(),
        })
        .await;

    // 帧转发（v2 紧凑帧头；静默超时兜底——同 handle_push_loop）
    loop {
        match tokio::time::timeout(
            PUSH_SILENCE_TIMEOUT,
            crate::transport::quic::read_media_frame(&mut rx),
        )
        .await
        {
            Ok(Ok(Some(frame))) => state.forward(&stream_id, frame),
            Ok(Ok(None)) => break,
            Ok(Err(e)) => {
                tracing::warn!("QUIC 推流流读取异常: {e}");
                break;
            }
            Err(_elapsed) => {
                tracing::warn!(
                    "QUIC 推流端超过 {}s 无消息，判定失联，拆除流: {stream_id}",
                    PUSH_SILENCE_TIMEOUT.as_secs()
                );
                break;
            }
        }
    }
    if state.remove(&stream_id) {
        state.emit(RelayEvent::StreamEnded {
            stream_id: stream_id.clone(),
        });
        tracing::info!("推流结束: {stream_id}");
    }
    let _ = link
        .send_control(stross_proto::message::ControlMessage::CloseStream { stream_id })
        .await;
}

/// QUIC 观看任务（每条媒体流一个）：OpenStream(watch) 已配对，订阅流广播
/// 通道 → 写紧凑帧到客户端媒体流；流结束 / 客户端断开收尾。
async fn quic_watch_stream(
    link: crate::transport::quic::QuicServerLink,
    state: RelayState,
    stream_id: StreamId,
    mut tx: quinn::SendStream,
    mut _rx: quinn::RecvStream,
) {
    let Some(entry) = state.get(&stream_id) else {
        let _ = link
            .send_control(stross_proto::message::ControlMessage::Error {
                message: format!("流 {stream_id} 不存在"),
            })
            .await;
        let _ = link
            .send_control(stross_proto::message::ControlMessage::CloseStream { stream_id })
            .await;
        return;
    };
    let info = entry.info.clone();
    let mut brx = entry.tx.subscribe();
    let last_kf = entry.last_keyframe.clone();
    let watchers = entry.tx.receiver_count() as u32;
    drop(entry);
    state.emit(RelayEvent::WatchersChanged {
        stream_id: stream_id.clone(),
        watchers,
    });
    let _ = link
        .send_control(stross_proto::message::ControlMessage::StreamOpened {
            stream_id: stream_id.clone(),
        })
        .await;

    // 补发最近关键帧（新观看者立即对齐 GOP；同 handle_watch）
    let mut video_started = false;
    if let Some(kf) = last_kf {
        if crate::transport::quic::write_media_frame(&mut tx, &kf)
            .await
            .is_err()
        {
            drop(brx);
            state.emit(RelayEvent::WatchersChanged {
                stream_id: stream_id.clone(),
                watchers: 0,
            });
            return;
        }
        video_started = true;
    }
    loop {
        let frame = match brx.recv().await {
            Ok(f) => f,
            Err(broadcast::error::RecvError::Lagged(_)) => {
                // 掉帧：重新等下一个关键帧，避免从 GOP 中间开始
                video_started = false;
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        };
        if frame.header.track == stross_proto::frame::TRACK_VIDEO
            && !video_started
            && !frame.header.is_keyframe()
            && !frame.header.is_config()
        {
            continue;
        }
        if frame.header.track == stross_proto::frame::TRACK_VIDEO
            && (frame.header.is_keyframe() || frame.header.is_config())
        {
            video_started = true;
        }
        if crate::transport::quic::write_media_frame(&mut tx, &frame)
            .await
            .is_err()
        {
            break;
        }
    }
    drop(brx);
    let watchers = state
        .get(&stream_id)
        .map_or(0, |e| e.tx.receiver_count() as u32);
    state.emit(RelayEvent::WatchersChanged {
        stream_id: stream_id.clone(),
        watchers,
    });
    let _ = link
        .send_control(stross_proto::message::ControlMessage::CloseStream {
            stream_id: stream_id.clone(),
        })
        .await;
    tracing::debug!("观看端断开: {stream_id} ({})", info.title);
}

/// 统一接入点（SRT/QUIC 监听用）：首条控制消息决定角色——
/// `Hello` = 推流（进入 [`handle_push_loop`]），`Watch` = 观看（[`handle_watch`]）。
pub(super) async fn handle_connect(session: Box<dyn DataSession>, state: RelayState) {
    let first = match session.recv().await {
        Ok(Some(pkt)) => pkt,
        Ok(None) => {
            tracing::warn!("首条消息前连接断开");
            let _ = session.close().await;
            return;
        }
        Err(e) => {
            tracing::warn!("首条消息接收失败: {e}");
            let _ = session.close().await;
            return;
        }
    };
    match first {
        SessionPacket::Control(ControlMessage::Hello { .. }) => {
            handle_push_loop(session, state, Some(first)).await;
        }
        SessionPacket::Control(ControlMessage::Watch { stream_id }) => {
            handle_watch(session, stream_id, state).await;
        }
        other => {
            tracing::warn!("首条消息既不是 Hello 也不是 Watch: {other:?}");
            let _ = session
                .send(SessionPacket::Control(ControlMessage::Error {
                    message: "首条消息必须是 Hello（推流）或 Watch（观看）".into(),
                }))
                .await;
            let _ = session.close().await;
        }
    }
}

/// 推流循环主体；`pending` 为接入点已消费的首包（Hello），WS 路径为 `None`。
async fn handle_push_loop(
    session: Box<dyn DataSession>,
    state: RelayState,
    mut pending: Option<SessionPacket>,
) {
    let mut stream_id: Option<StreamId> = None;
    loop {
        let pkt = match pending.take() {
            Some(pkt) => pkt,
            None => match tokio::time::timeout(PUSH_SILENCE_TIMEOUT, session.recv()).await {
                Err(_elapsed) => {
                    tracing::warn!(
                        "推流端超过 {}s 无任何消息，判定失联（进程被 kill/断网），拆除流",
                        PUSH_SILENCE_TIMEOUT.as_secs()
                    );
                    break;
                }
                Ok(Ok(Some(pkt))) => pkt,
                Ok(Ok(None)) => {
                    tracing::warn!("推流端连接已断开（无更多消息）");
                    break;
                }
                Ok(Err(e)) => {
                    tracing::warn!("推流端连接异常: {e}");
                    break;
                }
            },
        };
        match pkt {
            SessionPacket::Control(ControlMessage::Hello {
                stream_id: id,
                title,
                video,
                audio,
                share_token,
            }) => {
                // 受控模式接入门控（需求 F2.2 + B 阶段凭证式跨设备推流）：
                // 回环来源（本机流程）走内核预授权；非回环 / 未知来源
                // （跨设备，如手机 → 电脑）必须出示有效接入凭证。
                if state.is_controlled() {
                    let local = session.peer_addr().is_some_and(|a| a.ip().is_loopback());
                    if !state.is_allowed(&id, share_token.as_deref(), local) {
                        tracing::warn!(
                            "推流被拒绝: 流 {id} 来源 {} 未授权（本机需先建会话，跨设备需出示接入凭证）",
                            if local { "回环" } else { "非回环" }
                        );
                        let _ = session
                            .send(SessionPacket::Control(ControlMessage::Error {
                                message: format!("流 {id} 未授权，请先创建会话或出示有效接入凭证"),
                            }))
                            .await;
                        let _ = session.close().await;
                        return;
                    }
                }
                // 同名流冲突时拒绝新推流端
                if state.get(&id).is_some() {
                    let _ = session
                        .send(SessionPacket::Control(ControlMessage::Error {
                            message: format!("流 {id} 已存在"),
                        }))
                        .await;
                    let _ = session.close().await;
                    return;
                }
                // 广播容量 128 ≈ 4 秒 @30fps：限制慢观看端的内存积压
                // （超出的观看端收到 Lagged，等下一个关键帧重对齐）
                let (tx, _rx) = broadcast::channel(128);
                let info = StreamInfo {
                    stream_id: id.clone(),
                    title,
                    video,
                    audio,
                    started_at: stross_proto::time::unix_secs(),
                    watchers: 0,
                };
                state.insert(StreamEntry {
                    info: info.clone(),
                    tx,
                    last_keyframe: None,
                });
                stream_id = Some(id.clone());
                state.emit(RelayEvent::StreamStarted {
                    stream_id: id.clone(),
                    info: info.clone(),
                });
                tracing::info!("推流开始: {} ({})", id, info.title);
                let _ = session
                    .send(SessionPacket::Control(ControlMessage::Welcome {
                        stream_id: id,
                    }))
                    .await;
            }
            SessionPacket::Control(ControlMessage::Bye) => {
                break;
            }
            SessionPacket::Control(_) => {}
            SessionPacket::Media(frame) => {
                if let Some(id) = &stream_id {
                    // 单次加锁完成关键帧缓存 + 广播（观看端自己按关键帧对齐）
                    state.forward(id, frame);
                }
            }
        }
    }
    if let Some(id) = stream_id {
        // 若流已被 revoke_stream 拆除（会话拆除路径），这里不再重复发事件
        if state.remove(&id) {
            state.emit(RelayEvent::StreamEnded {
                stream_id: id.clone(),
            });
            tracing::info!("推流结束: {id}");
        }
    }
    let _ = session.close().await;
}

/// 代理拉取任务：以观看者身份连接上游中继，把收到的帧转发到本地代理流；
/// 上游断开 / 连接失败时清理本地代理流（[`RelayState::remove_proxy`]）。
pub(super) async fn proxy_uplink(state: RelayState, upstream: String, stream_id: StreamId) {
    let session = match crate::watch::connect_watch(&upstream, &stream_id).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("代理上游连接失败 {upstream} {stream_id}: {e}");
            state.remove_proxy(&stream_id);
            return;
        }
    };
    tracing::info!("代理已连接上游: {upstream} → {stream_id}");
    loop {
        let pkt = match tokio::time::timeout(PUSH_SILENCE_TIMEOUT, session.recv()).await {
            Err(_elapsed) => {
                tracing::warn!(
                    "代理上游超过 {}s 无消息，判定失联，清理本地流: {stream_id}",
                    PUSH_SILENCE_TIMEOUT.as_secs()
                );
                break;
            }
            Ok(Ok(Some(pkt))) => pkt,
            Ok(Ok(None)) => break,
            Ok(Err(e)) => {
                tracing::warn!("代理上游接收异常: {e}");
                break;
            }
        };
        match pkt {
            SessionPacket::Media(frame) => state.forward(&stream_id, frame),
            SessionPacket::Control(_) => continue,
        }
    }
    tracing::info!("代理上游断开，清理本地流: {stream_id}");
    state.remove_proxy(&stream_id);
}
