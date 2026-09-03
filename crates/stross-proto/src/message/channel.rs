//! 节点间对等通道协议（即时消息、双向文件互传与通道信令）。
//!
//! 支持聊天/便签文本与流式文件分块互传（全双工通道），跑在无损传输（WS / QUIC）上。
//!
//! **代码规范铁律**：严禁使用裸 `String` 作 key / id；传输任务与消息一律使用强类型
//! 数值新类型 [`TransferId`] 与 [`MsgId`]。

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// 文件传输任务强类型 ID（纯数值新类型，严禁使用字符串作 key）。
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    ToSchema,
    Default,
)]
#[serde(transparent)]
pub struct TransferId(pub u32);

impl TransferId {
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

impl From<u32> for TransferId {
    fn from(id: u32) -> Self {
        Self(id)
    }
}

impl std::fmt::Display for TransferId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 消息/便签强类型 ID（纯数值序号/时间戳，严禁使用字符串作 key）。
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    ToSchema,
    Default,
)]
#[serde(transparent)]
pub struct MsgId(pub u64);

impl MsgId {
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl From<u64> for MsgId {
    fn from(id: u64) -> Self {
        Self(id)
    }
}

impl std::fmt::Display for MsgId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 双向节点通道消息（即时消息、文件互传与信令）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ChannelMsg {
    /// 文本消息（聊天、便签、剪贴板文本）
    #[serde(rename_all = "camelCase")]
    Text {
        msg_id: MsgId,
        text: String,
        timestamp: u64,
    },
    /// 发送端提议发送一个文件
    #[serde(rename_all = "camelCase")]
    FileOffer {
        transfer_id: TransferId,
        name: String,
        size: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sha256: Option<String>,
    },
    /// 接收端决策：同意或拒绝接收文件
    #[serde(rename_all = "camelCase")]
    FileDecision {
        transfer_id: TransferId,
        accept: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// 传输进度通告（可选低频心跳）
    #[serde(rename_all = "camelCase")]
    FileProgress {
        transfer_id: TransferId,
        transferred: u64,
        total: u64,
    },
    /// 取消/中断文件传输
    #[serde(rename_all = "camelCase")]
    FileCancel {
        transfer_id: TransferId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// 文件传输完成且校验落盘成功的确认
    #[serde(rename_all = "camelCase")]
    FileAck {
        transfer_id: TransferId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        save_path: Option<String>,
    },
    Ping,
    Pong,
}

/// 二进制文件分块传输头（固定 20 字节，小端序）。
///
/// 结构：
/// ```text
/// +---------------+---------------+-----------+-------+----------+
/// | transfer_id   | offset        | chunk_len | flags | reserved |
/// | u32 LE        | u64 LE        | u32 LE    | u8    | u8[3]    |
/// +---------------+---------------+-----------+-------+----------+
/// | 4             | 8             | 4         | 1     | 3        |
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelChunkHeader {
    /// 强类型传输 ID（与 FileOffer.transfer_id 完全一致，零哈希、零开销）
    pub transfer_id: TransferId,
    /// 在整个文件中的偏移字节
    pub offset: u64,
    /// 本块有效载荷长度
    pub chunk_len: u32,
    /// 标志（0x01 = 末块 EOF）
    pub flags: u8,
}

pub const CHANNEL_CHUNK_HEADER_LEN: usize = 20;

impl ChannelChunkHeader {
    /// 标志位：末块 EOF
    pub const FLAG_EOF: u8 = 0x01;

    /// 编码为 20 字节二进制
    pub fn encode(&self) -> [u8; CHANNEL_CHUNK_HEADER_LEN] {
        let mut buf = [0u8; CHANNEL_CHUNK_HEADER_LEN];
        buf[0..4].copy_from_slice(&self.transfer_id.0.to_le_bytes());
        buf[4..12].copy_from_slice(&self.offset.to_le_bytes());
        buf[12..16].copy_from_slice(&self.chunk_len.to_le_bytes());
        buf[16] = self.flags;
        buf
    }

    /// 从切片解码（长度不足返回 None）
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < CHANNEL_CHUNK_HEADER_LEN {
            return None;
        }
        let transfer_num = u32::from_le_bytes(buf[0..4].try_into().ok()?);
        let offset = u64::from_le_bytes(buf[4..12].try_into().ok()?);
        let chunk_len = u32::from_le_bytes(buf[12..16].try_into().ok()?);
        let flags = buf[16];
        Some(Self {
            transfer_id: TransferId(transfer_num),
            offset,
            chunk_len,
            flags,
        })
    }

    /// 是否是最后一包
    pub fn is_eof(&self) -> bool {
        (self.flags & Self::FLAG_EOF) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_channel_chunk_header_roundtrip() {
        let h = ChannelChunkHeader {
            transfer_id: TransferId(42),
            offset: 1024 * 1024 * 5,
            chunk_len: 65536,
            flags: ChannelChunkHeader::FLAG_EOF,
        };
        let bytes = h.encode();
        assert_eq!(bytes.len(), CHANNEL_CHUNK_HEADER_LEN);
        let decoded = ChannelChunkHeader::decode(&bytes).expect("decode failed");
        assert_eq!(h, decoded);
        assert_eq!(decoded.transfer_id, TransferId(42));
        assert!(decoded.is_eof());
    }

    #[test]
    fn test_channel_msg_json() {
        let msg = ChannelMsg::Text {
            msg_id: MsgId(1001),
            text: "你好".into(),
            timestamp: 1700000000,
        };
        let s = serde_json::to_string(&msg).unwrap();
        assert!(s.contains("\"type\":\"text\""));
        assert!(s.contains("\"msgId\":1001"));
        assert!(s.contains("\"text\":\"你好\""));
        let de: ChannelMsg = serde_json::from_str(&s).unwrap();
        assert_eq!(msg, de);

        let offer = ChannelMsg::FileOffer {
            transfer_id: TransferId(5),
            name: "test.zip".into(),
            size: 1048576,
            mime: Some("application/zip".into()),
            sha256: None,
        };
        let s2 = serde_json::to_string(&offer).unwrap();
        assert!(s2.contains("\"type\":\"fileOffer\""));
        assert!(s2.contains("\"transferId\":5"));
        let de2: ChannelMsg = serde_json::from_str(&s2).unwrap();
        assert_eq!(offer, de2);
    }
}
