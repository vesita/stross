//! 单个对等节点的双向通道会话（全双工文字与文件互传）。
//!
//! **代码规范铁律**：严禁使用裸 `String` 作 key / id；传输任务与消息一律使用强类型
//! 数值新类型 [`TransferId`] 与 [`MsgId`]。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, broadcast, oneshot};

use stross_proto::frame::{CODEC_CHANNEL, Frame, TRACK_CHANNEL};
use stross_proto::message::{
    CHANNEL_CHUNK_HEADER_LEN, ChannelChunkHeader, ChannelMsg, ControlMessage, MsgId, TransferId,
};
use stross_proto::time::unix_secs;
use stross_transport::{DataSession, SessionPacket};
use stross_view::channel::ChannelEvent;

use crate::kernel::id::Id;

/// 单块传输尺寸（64 KiB；与 WS/QUIC 缓冲友好）。
pub const FILE_CHUNK_SIZE: usize = 64 * 1024;

/// 入站文件接收任务。
struct InboundTransfer {
    name: String,
    size: u64,
    transferred: u64,
    tmp_path: PathBuf,
    file: tokio::fs::File,
}

/// 出站文件推送任务句柄。
struct OutboundTransfer {
    decision_tx: Option<oneshot::Sender<bool>>,
    ack_tx: Option<oneshot::Sender<()>>,
    cancelled: Arc<AtomicBool>,
}

/// 对等节点通道会话（全双工）。
pub struct ChannelSession {
    pub peer_id: Id,
    pub peer_name: String,
    session: Arc<dyn DataSession>,
    out_dir: PathBuf,
    auto_accept: bool,
    events_tx: broadcast::Sender<ChannelEvent>,
    next_msg_id: AtomicU64,
    next_transfer_id: AtomicU32,
    inbounds: Mutex<HashMap<TransferId, InboundTransfer>>,
    outbounds: Mutex<HashMap<TransferId, OutboundTransfer>>,
    closed: AtomicBool,
    closed_notify: tokio::sync::Notify,
}

impl ChannelSession {
    /// 构造新通道会话并启动后台接收循环。
    pub fn new(
        peer_id: Id,
        peer_name: String,
        session: Box<dyn DataSession>,
        out_dir: PathBuf,
        auto_accept: bool,
        events_tx: broadcast::Sender<ChannelEvent>,
    ) -> Arc<Self> {
        let session_arc: Arc<dyn DataSession> = Arc::from(session);
        let this = Arc::new(Self {
            peer_id: peer_id.clone(),
            peer_name: peer_name.clone(),
            session: session_arc.clone(),
            out_dir,
            auto_accept,
            events_tx: events_tx.clone(),
            next_msg_id: AtomicU64::new(1),
            next_transfer_id: AtomicU32::new(1),
            inbounds: Mutex::new(HashMap::new()),
            outbounds: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            closed_notify: tokio::sync::Notify::new(),
        });

        // 广播连接建立事件
        let _ = events_tx.send(ChannelEvent::Connected {
            peer_id: peer_id.to_string(),
            peer_name,
        });

        // 启动后台事件与帧读取循环
        let self_weak = Arc::downgrade(&this);
        tokio::spawn(async move {
            Self::receive_loop(self_weak, session_arc).await;
        });

        this
    }

    /// 会话是否仍然存活。
    pub fn is_alive(&self) -> bool {
        !self.closed.load(Ordering::Relaxed)
    }

    /// 等待会话结束（服务端挂起 axum on_upgrade 用）。
    pub async fn wait_closed(&self) {
        if self.closed.load(Ordering::Relaxed) {
            return;
        }
        self.closed_notify.notified().await;
    }
    /// 主动发送文本消息（聊天/便签/剪贴板文字）。
    pub async fn send_text(&self, text: &str) -> Result<MsgId> {
        if self.closed.load(Ordering::Relaxed) {
            bail!("通道已关闭");
        }
        let msg_id = MsgId::new(self.next_msg_id.fetch_add(1, Ordering::Relaxed));
        let timestamp = unix_secs();
        let msg = ChannelMsg::Text {
            msg_id,
            text: text.to_string(),
            timestamp,
        };
        self.session
            .send(SessionPacket::Control(ControlMessage::Channel { msg }))
            .await
            .context("发送通道文本消息失败")?;

        // 广播本机发送成功事件
        let _ = self.events_tx.send(ChannelEvent::Message {
            msg_id,
            text: text.to_string(),
            timestamp,
            is_self: true,
            peer_id: self.peer_id.to_string(),
        });
        Ok(msg_id)
    }

    /// 主动发送本地文件到对端（全双工流式推送）。
    pub async fn send_file(&self, path: &Path) -> Result<TransferId> {
        if self.closed.load(Ordering::Relaxed) {
            bail!("通道已关闭");
        }
        let meta = tokio::fs::metadata(path)
            .await
            .with_context(|| format!("读取文件元数据失败 {}", path.display()))?;
        if !meta.is_file() {
            bail!("不是普通文件: {}", path.display());
        }
        let size = meta.len();
        let name = path
            .file_name()
            .map_or_else(|| "未命名".into(), |s| s.to_string_lossy().to_string());
        let transfer_id = TransferId::new(self.next_transfer_id.fetch_add(1, Ordering::Relaxed));

        let (dec_tx, dec_rx) = oneshot::channel();
        let (ack_tx, ack_rx) = oneshot::channel();
        let cancelled = Arc::new(AtomicBool::new(false));

        {
            let mut out = self.outbounds.lock().await;
            out.insert(
                transfer_id,
                OutboundTransfer {
                    decision_tx: Some(dec_tx),
                    ack_tx: Some(ack_tx),
                    cancelled: cancelled.clone(),
                },
            );
        }

        // 发送 Offer
        let offer = ChannelMsg::FileOffer {
            transfer_id,
            name: name.clone(),
            size,
            mime: None,
            sha256: None,
        };
        self.session
            .send(SessionPacket::Control(ControlMessage::Channel {
                msg: offer,
            }))
            .await
            .context("发送文件传输 Offer 失败")?;

        let _ = self.events_tx.send(ChannelEvent::FileOffer {
            transfer_id,
            name: name.clone(),
            size,
            peer_id: self.peer_id.to_string(),
        });

        // 启动后台推送任务
        let session = self.session.clone();
        let events_tx = self.events_tx.clone();
        let path_owned = path.to_path_buf();
        let name_owned = name.clone();

        tokio::spawn(async move {
            // 等待对端同意
            let accepted = matches!(dec_rx.await, Ok(true));
            if !accepted {
                let _ = events_tx.send(ChannelEvent::FileFailed {
                    transfer_id,
                    reason: "对端拒绝接收文件".into(),
                });
                return;
            }

            // 打开文件按块推送
            let mut f = match tokio::fs::File::open(&path_owned).await {
                Ok(f) => f,
                Err(e) => {
                    let _ = events_tx.send(ChannelEvent::FileFailed {
                        transfer_id,
                        reason: format!("读取文件失败: {e}"),
                    });
                    return;
                }
            };

            let mut buf = vec![0u8; FILE_CHUNK_SIZE];
            let mut offset: u64 = 0;

            loop {
                if cancelled.load(Ordering::Relaxed) {
                    // cancel_transfer 已主动发出 FileFailed 事件，无需重复发送
                    return;
                }

                let n = match f.read(&mut buf).await {
                    Ok(n) => n,
                    Err(e) => {
                        let _ = events_tx.send(ChannelEvent::FileFailed {
                            transfer_id,
                            reason: format!("读取文件块失败: {e}"),
                        });
                        return;
                    }
                };

                let is_eof = (n == 0) || (offset + n as u64 >= size) || (size == 0);
                let flags = if is_eof {
                    ChannelChunkHeader::FLAG_EOF
                } else {
                    0
                };

                let header = ChannelChunkHeader {
                    transfer_id,
                    offset,
                    chunk_len: n as u32,
                    flags,
                };

                let mut payload = Vec::with_capacity(CHANNEL_CHUNK_HEADER_LEN + n);
                payload.extend_from_slice(&header.encode());
                if n > 0 {
                    payload.extend_from_slice(&buf[..n]);
                }

                let frame = Frame::new(
                    TRACK_CHANNEL,
                    CODEC_CHANNEL,
                    flags,
                    transfer_id.as_u32(),
                    Bytes::from(payload),
                );

                if let Err(e) = session.send(SessionPacket::Media(frame)).await {
                    let _ = events_tx.send(ChannelEvent::FileFailed {
                        transfer_id,
                        reason: format!("发送数据块失败: {e}"),
                    });
                    return;
                }

                offset += n as u64;
                let _ = events_tx.send(ChannelEvent::FileProgress {
                    transfer_id,
                    transferred: offset,
                    total: size,
                    is_upload: true,
                });

                if is_eof {
                    break;
                }
            }

            // 等待对端 Ack 确认
            match ack_rx.await {
                Ok(()) => {
                    let _ = events_tx.send(ChannelEvent::FileCompleted {
                        transfer_id,
                        name: name_owned,
                        path: None,
                        is_upload: true,
                    });
                }
                Err(_) => {
                    let _ = events_tx.send(ChannelEvent::FileFailed {
                        transfer_id,
                        reason: "未收到对端完成确认".into(),
                    });
                }
            }
        });

        Ok(transfer_id)
    }

    /// 取消某次传输任务。
    pub async fn cancel_transfer(&self, transfer_id: TransferId) -> Result<()> {
        let mut was_active = false;
        {
            let mut out = self.outbounds.lock().await;
            if let Some(t) = out.remove(&transfer_id) {
                t.cancelled.store(true, Ordering::Relaxed);
                was_active = true;
            }
        }
        {
            let mut inb = self.inbounds.lock().await;
            if let Some(t) = inb.remove(&transfer_id) {
                let _ = tokio::fs::remove_file(&t.tmp_path).await;
                was_active = true;
            }
        }

        if !was_active {
            return Ok(());
        }

        let cancel = ChannelMsg::FileCancel {
            transfer_id,
            reason: Some("用户主动取消".into()),
        };
        let _ = self
            .session
            .send(SessionPacket::Control(ControlMessage::Channel {
                msg: cancel,
            }))
            .await;

        let _ = self.events_tx.send(ChannelEvent::FileFailed {
            transfer_id,
            reason: "传输已取消".into(),
        });
        Ok(())
    }

    /// 关闭会话。
    pub async fn close(&self) -> Result<()> {
        if !self.closed.swap(true, Ordering::Relaxed) {
            self.closed_notify.notify_waiters();
            let _ = self.session.close().await;

            // 清理未完成的在途接收临时文件，防止垃圾残留
            {
                let mut inb = self.inbounds.lock().await;
                for (_, t) in inb.drain() {
                    let _ = tokio::fs::remove_file(&t.tmp_path).await;
                }
            }
            // 标记在途上传任务已取消
            {
                let mut out = self.outbounds.lock().await;
                for (_, t) in out.drain() {
                    t.cancelled.store(true, Ordering::Relaxed);
                }
            }

            let _ = self.events_tx.send(ChannelEvent::Disconnected {
                peer_id: self.peer_id.to_string(),
            });
        }
        Ok(())
    }
    /// 后台消息与分块读取循环。
    async fn receive_loop(self_weak: std::sync::Weak<Self>, session: Arc<dyn DataSession>) {
        loop {
            match session.recv().await {
                Ok(Some(SessionPacket::Control(ControlMessage::Channel { msg }))) => {
                    let Some(this) = self_weak.upgrade() else {
                        break;
                    };
                    this.handle_control_msg(msg).await;
                }
                Ok(Some(SessionPacket::Media(frame))) if frame.header.track == TRACK_CHANNEL => {
                    let Some(this) = self_weak.upgrade() else {
                        break;
                    };
                    this.handle_media_frame(frame).await;
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {
                    if let Some(this) = self_weak.upgrade() {
                        let _ = this.close().await;
                    }
                    break;
                }
            }
        }
    }

    /// 处理控制消息。
    async fn handle_control_msg(&self, msg: ChannelMsg) {
        match msg {
            ChannelMsg::Text {
                msg_id,
                text,
                timestamp,
            } => {
                let _ = self.events_tx.send(ChannelEvent::Message {
                    msg_id,
                    text,
                    timestamp,
                    is_self: false,
                    peer_id: self.peer_id.to_string(),
                });
            }
            ChannelMsg::FileOffer {
                transfer_id,
                name,
                size,
                ..
            } => {
                let _ = self.events_tx.send(ChannelEvent::FileOffer {
                    transfer_id,
                    name: name.clone(),
                    size,
                    peer_id: self.peer_id.to_string(),
                });

                if self.auto_accept
                    && let Err(e) = self.accept_inbound(transfer_id, name, size).await
                {
                    tracing::error!("接受文件失败: {e}");
                }
            }
            ChannelMsg::FileDecision {
                transfer_id,
                accept,
                ..
            } => {
                let mut out = self.outbounds.lock().await;
                if let Some(mut t) = out.remove(&transfer_id) {
                    if let Some(dec) = t.decision_tx.take() {
                        let _ = dec.send(accept);
                    }
                    if accept {
                        out.insert(transfer_id, t);
                    }
                }
            }
            ChannelMsg::FileProgress {
                transfer_id,
                transferred,
                total,
            } => {
                let _ = self.events_tx.send(ChannelEvent::FileProgress {
                    transfer_id,
                    transferred,
                    total,
                    is_upload: false,
                });
            }
            ChannelMsg::FileCancel {
                transfer_id,
                reason,
            } => {
                let r = reason.unwrap_or_else(|| "对端取消传输".into());
                {
                    let mut inb = self.inbounds.lock().await;
                    if let Some(t) = inb.remove(&transfer_id) {
                        let _ = tokio::fs::remove_file(&t.tmp_path).await;
                    }
                }
                {
                    let mut out = self.outbounds.lock().await;
                    if let Some(t) = out.remove(&transfer_id) {
                        t.cancelled.store(true, Ordering::Relaxed);
                    }
                }
                let _ = self.events_tx.send(ChannelEvent::FileFailed {
                    transfer_id,
                    reason: r,
                });
            }
            ChannelMsg::FileAck { transfer_id, .. } => {
                let mut out = self.outbounds.lock().await;
                if let Some(mut t) = out.remove(&transfer_id)
                    && let Some(ack) = t.ack_tx.take()
                {
                    let _ = ack.send(());
                }
            }
            ChannelMsg::Ping => {
                let _ = self
                    .session
                    .send(SessionPacket::Control(ControlMessage::Channel {
                        msg: ChannelMsg::Pong,
                    }))
                    .await;
            }
            ChannelMsg::Pong => {}
        }
    }

    /// 接受对端推送的文件并准备临时文件。
    async fn accept_inbound(&self, transfer_id: TransferId, name: String, size: u64) -> Result<()> {
        let _ = tokio::fs::create_dir_all(&self.out_dir).await;
        let safe_name = sanitize_file_name(&name);
        let tmp_name = format!(".tmp-recv-{}-{}", transfer_id, unix_secs());
        let tmp_path = self.out_dir.join(tmp_name);
        let file = tokio::fs::File::create(&tmp_path)
            .await
            .with_context(|| format!("创建临时文件失败 {}", tmp_path.display()))?;

        {
            let mut inb = self.inbounds.lock().await;
            inb.insert(
                transfer_id,
                InboundTransfer {
                    name: safe_name,
                    size,
                    transferred: 0,
                    tmp_path,
                    file,
                },
            );
        }
        // 发送同意决策
        let dec = ChannelMsg::FileDecision {
            transfer_id,
            accept: true,
            reason: None,
        };
        self.session
            .send(SessionPacket::Control(ControlMessage::Channel { msg: dec }))
            .await
            .context("发送同意文件决策失败")?;
        Ok(())
    }

    /// 处理到达的文件数据块媒体帧。
    async fn handle_media_frame(&self, frame: Frame) {
        let Some(header) = ChannelChunkHeader::decode(&frame.payload) else {
            return;
        };
        let chunk_len = header.chunk_len as usize;
        if frame.payload.len() < CHANNEL_CHUNK_HEADER_LEN + chunk_len {
            tracing::warn!(
                "文件分块载荷长度不足: 期望 {chunk_len}, 实际载荷 {}",
                frame.payload.len()
            );
            return;
        }
        let chunk_data =
            &frame.payload[CHANNEL_CHUNK_HEADER_LEN..CHANNEL_CHUNK_HEADER_LEN + chunk_len];

        let mut inb = self.inbounds.lock().await;
        let Some(t) = inb.get_mut(&header.transfer_id) else {
            return;
        };

        // 防御流式载荷溢出（单任务超出声明尺寸容限，防磁盘耗尽 Dos）
        if t.transferred + chunk_data.len() as u64 > t.size.saturating_add(1024 * 1024) {
            tracing::error!(
                "文件块传输超出声明大小，中止传输: id={}",
                header.transfer_id
            );
            let transfer_id = header.transfer_id;
            if let Some(t) = inb.remove(&transfer_id) {
                let _ = tokio::fs::remove_file(&t.tmp_path).await;
            }
            drop(inb);
            let _ = self.cancel_transfer(transfer_id).await;
            return;
        }

        if !chunk_data.is_empty() {
            if let Err(e) = t.file.write_all(chunk_data).await {
                tracing::error!("写入临时文件失败: {e}");
                return;
            }
            t.transferred += chunk_data.len() as u64;
        }

        let transferred = t.transferred;
        let total = t.size;
        let transfer_id = header.transfer_id;

        let _ = self.events_tx.send(ChannelEvent::FileProgress {
            transfer_id,
            transferred,
            total,
            is_upload: false,
        });

        if header.is_eof() {
            // 完成落盘，释放文件句柄并重命名
            let _ = t.file.flush().await;
            let tmp_path = t.tmp_path.clone();
            let name = t.name.clone();
            inb.remove(&transfer_id);
            drop(inb);

            let final_path = resolve_unique_path(&self.out_dir, &name);
            if let Err(e) = tokio::fs::rename(&tmp_path, &final_path).await {
                tracing::error!("重命名接收文件失败: {e}");
                let _ = tokio::fs::remove_file(&tmp_path).await;
                let _ = self.events_tx.send(ChannelEvent::FileFailed {
                    transfer_id,
                    reason: format!("落盘重命名失败: {e}"),
                });
                return;
            }

            let path_str = final_path.display().to_string();
            // 发送确认
            let ack = ChannelMsg::FileAck {
                transfer_id,
                save_path: Some(path_str.clone()),
            };
            let _ = self
                .session
                .send(SessionPacket::Control(ControlMessage::Channel { msg: ack }))
                .await;

            let _ = self.events_tx.send(ChannelEvent::FileCompleted {
                transfer_id,
                name,
                path: Some(path_str),
                is_upload: false,
            });
        }
    }
}

/// 清洗不可信对端发来的文件名，彻底防御路径穿越（Path Traversal）与保留名注入。
pub fn sanitize_file_name(raw_name: &str) -> String {
    // 跨平台提取最末端文件名：同时兼容 POSIX ('/') 与 Windows ('\\') 分隔符
    let normalized = raw_name.replace('\\', "/");
    let file_name = Path::new(&normalized)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let sanitized: String = file_name
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();

    let trimmed = sanitized.trim().trim_matches('.');
    if trimmed.is_empty() || trimmed == ".." || trimmed == "." {
        return format!("file_{}", unix_secs());
    }
    trimmed.to_string()
}

/// 解析唯一落盘文件名（同名自动加 (1), (2)），确保落盘路径绝不越界。
fn resolve_unique_path(dir: &Path, name: &str) -> PathBuf {
    let safe_name = sanitize_file_name(name);
    let base = dir.join(&safe_name);
    if !base.starts_with(dir) {
        return dir.join(format!("file_{}", unix_secs()));
    }
    if !base.exists() {
        return base;
    }
    let p = Path::new(&safe_name);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or(&safe_name);
    let ext = p.extension().and_then(|e| e.to_str());
    let mut i = 1;
    loop {
        let new_name = match ext {
            Some(e) => format!("{stem} ({i}).{e}"),
            None => format!("{stem} ({i})"),
        };
        let candidate = dir.join(new_name);
        if !candidate.exists() {
            return candidate;
        }
        i += 1;
    }
}
