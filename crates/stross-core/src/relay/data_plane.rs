//! 数据面转发（传输无关；HTTP / SRT / QUIC 共用）。
//!
//! 本模块是中继的数据面热路径：把入站 [`DataSession`] 会话接入流表
//! （推流 / 观看 / 级联代理转发），只依赖
//! [`Transport`](crate::transport::Transport) 抽象，不感知具体传输；
//! 共享状态（流表 / 授权 / 事件）统一存于 [`RelayState`]（[`super`]）。
//!
//! 接入点：
//!
//! * [`handle_push`] / [`handle_watch`]：WebSocket 升级处直接调用（[`super::http`]）
//! * [`handle_connect`]：SRT / QUIC 监听统一入口，按首条控制消息分流
//!   （`Hello` = 推流，`Watch` = 观看）
//! * [`spawn_accept_loop`]：SRT / QUIC 通用入站 accept 循环（消除复制）
//! * [`proxy_uplink`]：级联代理拉取任务（上游中继 → 本地代理流）
//!
//! 观看端接入时机：视频只在关键帧（IDR）后开始转发（ffmpeg 已在关键帧前重复
//! SPS/PPS，因此等待关键帧即可）；音频 ADTS 自带配置，可直接转发。

use stross_proto::frame::TRACK_VIDEO;
use stross_proto::message::{ControlMessage, StreamInfo};
use tokio::sync::broadcast;

use crate::transport::{DataSession, SessionPacket};

use super::{RelayEvent, RelayState, StreamEntry};

/// 观看端：等待 Ready，然后按关键帧对齐转发。
pub(super) async fn handle_watch(
    session: Box<dyn DataSession>,
    stream_id: String,
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
        .map(|e| e.tx.receiver_count() as u32)
        .unwrap_or(0);
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

#[async_trait::async_trait]
impl AcceptLoop for crate::transport::quic::QuicListenerHandle {
    async fn accept_session(
        &mut self,
    ) -> Result<Box<dyn DataSession>, crate::transport::TransportError> {
        self.accept().await
    }
}

/// 通用入站 accept 循环：接收入站连接交给 [`handle_connect`]；
/// 监听错误或停机信号退出。SRT / QUIC 两个 UDP 监听共用（消除复制）。
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
    let mut stream_id: Option<String> = None;
    loop {
        let pkt = match pending.take() {
            Some(pkt) => pkt,
            None => match session.recv().await {
                Ok(Some(pkt)) => pkt,
                Ok(None) => {
                    tracing::warn!("推流端连接已断开（无更多消息）");
                    break;
                }
                Err(e) => {
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
                    let local = session
                        .peer_addr()
                        .map(|a| a.ip().is_loopback())
                        .unwrap_or(false);
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
pub(super) async fn proxy_uplink(state: RelayState, upstream: String, stream_id: String) {
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
        match session.recv().await {
            Ok(Some(SessionPacket::Media(frame))) => state.forward(&stream_id, frame),
            Ok(Some(SessionPacket::Control(_))) => continue,
            Ok(None) => break,
            Err(e) => {
                tracing::warn!("代理上游接收异常: {e}");
                break;
            }
        }
    }
    tracing::info!("代理上游断开，清理本地流: {stream_id}");
    state.remove_proxy(&stream_id);
}
