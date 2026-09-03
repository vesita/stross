//! 节点对等通道与文件互传应用契约 DTO（跨壳层单一真源）。
//!
//! 供 GUI 前端事件、CLI 状态展示与内核通知统一消费。

use serde::{Deserialize, Serialize};
use stross_proto::message::{MsgId, TransferId};

/// 通道事件（广播给前端或上层监听器）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "camelCase")]
pub enum ChannelEvent {
    /// 与对端节点建立全双工通道
    Connected { peer_id: String, peer_name: String },
    /// 通道断开
    Disconnected { peer_id: String },
    /// 收到文本消息（聊天、便签、复制文本）
    Message {
        msg_id: MsgId,
        text: String,
        timestamp: u64,
        is_self: bool,
        peer_id: String,
    },
    /// 收到文件传输提议
    FileOffer {
        transfer_id: TransferId,
        name: String,
        size: u64,
        peer_id: String,
    },
    /// 文件传输进度更新
    FileProgress {
        transfer_id: TransferId,
        transferred: u64,
        total: u64,
        is_upload: bool,
    },
    /// 文件传输完成
    FileCompleted {
        transfer_id: TransferId,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        is_upload: bool,
    },
    /// 文件传输失败或取消
    FileFailed {
        transfer_id: TransferId,
        reason: String,
    },
}

/// 活跃通道状态概览（GUI/CLI 查询用）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChannelStatus {
    pub peer_id: String,
    pub peer_name: String,
    pub connected: bool,
    pub active_transfers: u32,
}
