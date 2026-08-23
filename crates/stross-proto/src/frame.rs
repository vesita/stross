//! 二进制媒体帧格式。
//!
//! 每个 WebSocket 二进制消息是一个完整的帧：
//!
//! ```text
//! +--------+---------+-------+-------+---------+---------+---------+
//! | magic  | version | track | codec | flags   | pts_ms  | len     | payload ... |
//! | "STR1" |  u8     |  u8   |  u8   |  u8     | u32 LE  | u32 LE  |
//! +--------+---------+-------+-------+---------+---------+---------+
//! | 4      | 1       | 1     | 1     | 1       | 4       | 4       | len
//! ```
//!
//! * `track`: 0=视频, 1=音频
//! * `codec`: 1=H.264 (Annex-B), 2=AAC (ADTS)
//! * `flags`:
//!   * `0x01` KEYFRAME —— 本帧是关键帧（IDR 访问单元）
//!   * `0x02` CONFIG   —— 本帧是解码器配置数据（如 SPS/PPS、AudioSpecificConfig）
//!   * `0x04` START    —— 推流会话开始
//!   * `0x08` END      —— 推流会话结束
//! * `pts_ms`: 演示时间戳（毫秒，相对会话起点）
//! * `len`: 载荷长度
//!
//! 头部小端序，共 16 字节，与平台无关。

use bytes::Bytes;
use thiserror::Error;

/// 魔数，用于快速校验帧完整性。
pub const MAGIC: &[u8; 4] = b"STR1";
/// 协议版本。
pub const VERSION: u8 = 1;

/// 头部固定长度。
pub const HEADER_LEN: usize = 16;

// ---- track ----
pub const TRACK_VIDEO: u8 = 0;
pub const TRACK_AUDIO: u8 = 1;

// ---- codec ----
pub const CODEC_H264: u8 = 1;
pub const CODEC_AAC: u8 = 2;

// ---- flags ----
pub const FLAG_KEYFRAME: u8 = 0x01;
pub const FLAG_CONFIG: u8 = 0x02;
pub const FLAG_START: u8 = 0x04;
pub const FLAG_END: u8 = 0x08;

/// 帧解析错误。
#[derive(Debug, Error)]
pub enum FrameError {
    #[error("帧太短：需要至少 {HEADER_LEN} 字节，实际 {0}")]
    TooShort(usize),
    #[error("魔数错误：{0:?}")]
    BadMagic([u8; 4]),
    #[error("不支持的协议版本：{0}")]
    BadVersion(u8),
}

/// 媒体帧头。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub track: u8,
    pub codec: u8,
    pub flags: u8,
    pub pts_ms: u32,
    pub len: u32,
}

impl FrameHeader {
    /// 把帧头编码为 16 字节缓冲区。
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut buf = [0u8; HEADER_LEN];
        buf[0..4].copy_from_slice(MAGIC);
        buf[4] = VERSION;
        buf[5] = self.track;
        buf[6] = self.codec;
        buf[7] = self.flags;
        buf[8..12].copy_from_slice(&self.pts_ms.to_le_bytes());
        buf[12..16].copy_from_slice(&self.len.to_le_bytes());
        buf
    }

    /// 从缓冲区解析帧头。
    pub fn decode(buf: &[u8]) -> Result<Self, FrameError> {
        if buf.len() < HEADER_LEN {
            return Err(FrameError::TooShort(buf.len()));
        }
        let magic: [u8; 4] = buf[0..4].try_into().unwrap();
        if &magic != MAGIC {
            return Err(FrameError::BadMagic(magic));
        }
        let version = buf[4];
        if version != VERSION {
            return Err(FrameError::BadVersion(version));
        }
        Ok(FrameHeader {
            track: buf[5],
            codec: buf[6],
            flags: buf[7],
            pts_ms: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
            len: u32::from_le_bytes(buf[12..16].try_into().unwrap()),
        })
    }

    pub fn is_keyframe(&self) -> bool {
        self.flags & FLAG_KEYFRAME != 0
    }
    pub fn is_config(&self) -> bool {
        self.flags & FLAG_CONFIG != 0
    }
    pub fn is_start(&self) -> bool {
        self.flags & FLAG_START != 0
    }
    pub fn is_end(&self) -> bool {
        self.flags & FLAG_END != 0
    }
}

/// 一帧媒体数据（头 + 载荷）。
#[derive(Debug, Clone)]
pub struct Frame {
    pub header: FrameHeader,
    pub payload: Bytes,
}

impl Frame {
    pub fn new(track: u8, codec: u8, flags: u8, pts_ms: u32, payload: impl Into<Bytes>) -> Self {
        let payload = payload.into();
        Frame {
            header: FrameHeader {
                track,
                codec,
                flags,
                pts_ms,
                len: payload.len() as u32,
            },
            payload,
        }
    }

    /// 编码为完整的线上消息（头 + 载荷）。
    pub fn to_bytes(&self) -> Bytes {
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.extend_from_slice(&self.header.encode());
        out.extend_from_slice(&self.payload);
        out.into()
    }

    /// 从线上消息解码；若头声明的长度超出输入则返回 `None`。
    pub fn from_bytes(buf: &[u8]) -> Result<Self, FrameError> {
        let header = FrameHeader::decode(buf)?;
        let total = HEADER_LEN + header.len as usize;
        if buf.len() < total {
            return Err(FrameError::TooShort(buf.len()));
        }
        Ok(Frame {
            header,
            payload: Bytes::copy_from_slice(&buf[HEADER_LEN..total]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = FrameHeader {
            track: TRACK_VIDEO,
            codec: CODEC_H264,
            flags: FLAG_KEYFRAME | FLAG_START,
            pts_ms: 1234,
            len: 5678,
        };
        let buf = h.encode();
        assert_eq!(buf.len(), HEADER_LEN);
        let h2 = FrameHeader::decode(&buf).unwrap();
        assert_eq!(h, h2);
        assert!(h2.is_keyframe());
        assert!(h2.is_start());
        assert!(!h2.is_end());
    }

    #[test]
    fn frame_roundtrip() {
        let f = Frame::new(TRACK_AUDIO, CODEC_AAC, 0, 42, vec![1u8, 2, 3, 4]);
        let bytes = f.to_bytes();
        assert_eq!(bytes.len(), HEADER_LEN + 4);
        let f2 = Frame::from_bytes(&bytes).unwrap();
        assert_eq!(f.header, f2.header);
        assert_eq!(f.payload.to_vec(), f2.payload.to_vec());
    }

    #[test]
    fn rejects_bad_magic() {
        let mut buf = Frame::new(TRACK_VIDEO, CODEC_H264, 0, 0, vec![]).to_bytes().to_vec();
        buf[0] = b'X';
        assert!(Frame::from_bytes(&buf).is_err());
    }

    #[test]
    fn rejects_short_buffer() {
        assert!(Frame::from_bytes(&[0u8; 4]).is_err());
    }
}
