//! 对等通道管理器（ChannelManager）：维护所有活跃节点的双向通道会话。
//!
//! **代码规范铁律**：严禁使用裸 `String` 作 key / id；会话索引用 [`Id`]，
//! 传输任务用 [`TransferId`]，消息序号用 [`MsgId`]。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::{Mutex, broadcast};

use stross_proto::message::{MsgId, TransferId};
use stross_transport::{DataSession, Transport};
use stross_view::channel::{ChannelEvent, ChannelStatus};

use crate::channel::session::ChannelSession;
use crate::kernel::id::Id;

/// 通道管理器。
pub struct ChannelManager {
    sessions: Mutex<HashMap<Id, Arc<ChannelSession>>>,
    events_tx: broadcast::Sender<ChannelEvent>,
    out_dir: Mutex<PathBuf>,
    auto_accept: bool,
}

impl ChannelManager {
    /// 构造新管理器。
    pub fn new(out_dir: PathBuf, auto_accept: bool) -> Self {
        let (events_tx, _) = broadcast::channel(256);
        Self {
            sessions: Mutex::new(HashMap::new()),
            events_tx,
            out_dir: Mutex::new(out_dir),
            auto_accept,
        }
    }

    /// 订阅全局通道事件流（Tauri 前端或 CLI 监听）。
    pub fn subscribe_events(&self) -> broadcast::Receiver<ChannelEvent> {
        self.events_tx.subscribe()
    }

    /// 获取广播发送端（供注入给单独会话）。
    pub fn events_sender(&self) -> broadcast::Sender<ChannelEvent> {
        self.events_tx.clone()
    }

    /// 注册一条新建立的节点会话（主动拨号或被动入站）。
    pub async fn register_session(
        &self,
        peer_id: Id,
        peer_name: &str,
        session: Box<dyn DataSession>,
    ) -> Arc<ChannelSession> {
        let out_dir = self.out_dir.lock().await.clone();
        let chan = ChannelSession::new(
            peer_id.clone(),
            peer_name.to_string(),
            session,
            out_dir,
            self.auto_accept,
            self.events_tx.clone(),
        );

        let mut sessions = self.sessions.lock().await;
        // 若已有旧会话则先关闭替换
        if let Some(old) = sessions.insert(peer_id, chan.clone()) {
            let _ = old.close().await;
        }
        chan
    }

    /// 主动向对端中继的 `/ws/channel` 拨号建立全双工通道。
    pub async fn connect_channel(
        &self,
        peer_relay_base: &str,
        self_id: &Id,
        self_name: &str,
        peer_id: Id,
        peer_name: &str,
    ) -> Result<Arc<ChannelSession>> {
        let ws_base = if peer_relay_base.starts_with("http://") {
            peer_relay_base.replacen("http://", "ws://", 1)
        } else if peer_relay_base.starts_with("https://") {
            peer_relay_base.replacen("https://", "wss://", 1)
        } else if !peer_relay_base.starts_with("ws://") && !peer_relay_base.starts_with("wss://") {
            format!("ws://{peer_relay_base}")
        } else {
            peer_relay_base.to_string()
        };

        let encoded_name = percent_encode(self_name);
        let url = format!("{ws_base}/ws/channel?peer_id={self_id}&peer_name={encoded_name}");

        let transport = stross_transport::ws::WsTransport::new();
        let peer_addr = stross_transport::PeerAddr {
            transport: stross_proto::message::TransportId::Ws,
            addr: url,
        };
        let params = stross_transport::SessionParams {
            session_id: stross_proto::message::StreamId::new(format!("chan-{self_id}-{peer_id}")),
            profile: stross_proto::message::ReliabilityProfile::Lossless,
        };

        let session = transport
            .connect(&peer_addr, &params)
            .await
            .with_context(|| format!("连接对端通道失败: {peer_relay_base}"))?;

        Ok(self.register_session(peer_id, peer_name, session).await)
    }

    /// 获取与某个对等节点的活跃会话。
    pub async fn get_session(&self, peer_id: &Id) -> Option<Arc<ChannelSession>> {
        let mut sessions = self.sessions.lock().await;
        if let Some(s) = sessions.get(peer_id) {
            if s.is_alive() {
                return Some(s.clone());
            } else {
                sessions.remove(peer_id);
            }
        }
        None
    }

    /// 列出所有当前在线通道状态。
    pub async fn list_statuses(&self) -> Vec<ChannelStatus> {
        let mut sessions = self.sessions.lock().await;
        sessions.retain(|_, s| s.is_alive());

        sessions
            .iter()
            .map(|(id, s)| ChannelStatus {
                peer_id: id.to_string(),
                peer_name: s.peer_name.clone(),
                connected: s.is_alive(),
                active_transfers: 0,
            })
            .collect()
    }

    /// 向指定对等节点发送文本消息。
    pub async fn send_text(&self, peer_id: &Id, text: &str) -> Result<MsgId> {
        let session = self
            .get_session(peer_id)
            .await
            .with_context(|| format!("节点 {peer_id} 的通道尚未连接"))?;
        session.send_text(text).await
    }

    /// 向指定对等节点发送文件。
    pub async fn send_file(&self, peer_id: &Id, path: &Path) -> Result<TransferId> {
        let session = self
            .get_session(peer_id)
            .await
            .with_context(|| format!("节点 {peer_id} 的通道尚未连接"))?;
        session.send_file(path).await
    }

    /// 取消某次文件传输。
    pub async fn cancel_transfer(&self, peer_id: &Id, transfer_id: TransferId) -> Result<()> {
        let session = self
            .get_session(peer_id)
            .await
            .with_context(|| format!("节点 {peer_id} 的通道尚未连接"))?;
        session.cancel_transfer(transfer_id).await
    }

    /// 主动断开某节点的通道连接。
    pub async fn close_session(&self, peer_id: &Id) -> Result<()> {
        let mut sessions = self.sessions.lock().await;
        if let Some(s) = sessions.remove(peer_id) {
            s.close().await?;
        }
        Ok(())
    }

    /// 更改默认文件下载保存目录。
    pub async fn set_out_dir(&self, out_dir: PathBuf) {
        *self.out_dir.lock().await = out_dir;
    }
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                use std::fmt::Write;
                let _ = write!(out, "%{:02X}", b);
            }
        }
    }
    out
}
