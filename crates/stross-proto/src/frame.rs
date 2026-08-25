//! 二进制媒体帧格式（协议 v2）。
//!
//! 每个二进制消息是一个完整的帧（WS 上即一个 Binary 消息；UDP 类传输按
//! `frag_*` 字段分片/重组，这是传输实现的事务）：
//!
//! ```text
//! +--------+---------+-------+-------+---------+---------+---------+----------+----------+----------+----------+
//! | magic  | version | track | codec | flags   | pts_ms  | seq     | frag_idx | frag_cnt | len      | reserved |
//! | "STR2" |  u8     |  u8   |  u8   |  u8     | u32 LE  | u32 LE  | u8       | u8       | u32 LE   | u8[2]    |
//! +--------+---------+-------+-------+---------+---------+---------+----------+----------+----------+----------+
//! | 4      | 1       | 1     | 1     | 1       | 4       | 4       | 1        | 1        | 4        | 2        |
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
//! * `seq`: 会话内单调递增帧序号——有损传输乱序检测与丢包统计；无损传输取 0
//! * `frag_idx` / `frag_cnt`: 分片位置/总数；`frag_cnt == 0` 表示未分片
//! * `len`: 载荷长度
//!
//! 头部小端序，共 24 字节，与平台无关。v2 在 WS 上取 `seq=0, frag_cnt=0`
//! 时语义与 v1 等价（见 docs/plugin-architecture.md §5）。

use bytes::Bytes;
use thiserror::Error;

/// 魔数，用于快速校验帧完整性。
pub const MAGIC: &[u8; 4] = b"STR2";
/// 协议版本。
pub const VERSION: u8 = 2;

/// 头部固定长度。
pub const HEADER_LEN: usize = 24;

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
    /// 会话内单调递增帧序号（有损传输用；无损传输为 0）。
    pub seq: u32,
    /// 分片位置（`frag_cnt == 0` 时无意义）。
    pub frag_idx: u8,
    /// 分片总数（`0` = 未分片）。
    pub frag_cnt: u8,
    pub len: u32,
}

impl FrameHeader {
    /// 把帧头编码为 24 字节缓冲区。
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut buf = [0u8; HEADER_LEN];
        buf[0..4].copy_from_slice(MAGIC);
        buf[4] = VERSION;
        buf[5] = self.track;
        buf[6] = self.codec;
        buf[7] = self.flags;
        buf[8..12].copy_from_slice(&self.pts_ms.to_le_bytes());
        buf[12..16].copy_from_slice(&self.seq.to_le_bytes());
        buf[16] = self.frag_idx;
        buf[17] = self.frag_cnt;
        buf[18..22].copy_from_slice(&self.len.to_le_bytes());
        // [22..24] reserved：留作 flags 扩展
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
            seq: u32::from_le_bytes(buf[12..16].try_into().unwrap()),
            frag_idx: buf[16],
            frag_cnt: buf[17],
            len: u32::from_le_bytes(buf[18..22].try_into().unwrap()),
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
    /// 是否未分片（`frag_cnt == 0`）。
    pub fn is_whole(&self) -> bool {
        self.frag_cnt == 0
    }
}

/// 一帧媒体数据（头 + 载荷）。
#[derive(Debug, Clone)]
pub struct Frame {
    pub header: FrameHeader,
    pub payload: Bytes,
}

impl Frame {
    /// 构造未分片帧（`seq = 0`，`frag_cnt = 0`）。
    pub fn new(track: u8, codec: u8, flags: u8, pts_ms: u32, payload: impl Into<Bytes>) -> Self {
        let payload = payload.into();
        Frame {
            header: FrameHeader {
                track,
                codec,
                flags,
                pts_ms,
                seq: 0,
                frag_idx: 0,
                frag_cnt: 0,
                len: payload.len() as u32,
            },
            payload,
        }
    }

    /// 构造带帧序号的帧（有损传输会话内单调递增）。
    pub fn with_seq(
        track: u8,
        codec: u8,
        flags: u8,
        pts_ms: u32,
        seq: u32,
        payload: impl Into<Bytes>,
    ) -> Self {
        let payload = payload.into();
        Frame {
            header: FrameHeader {
                track,
                codec,
                flags,
                pts_ms,
                seq,
                frag_idx: 0,
                frag_cnt: 0,
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
    ///
    /// 拷贝语义：输入是借用切片，载荷会 `copy_from_slice` 复制一份。
    /// 热路径（WS/QUIC 接收）请用 [`Frame::from_bytes_owned`] 避免每帧全量拷贝。
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

    /// 从线上消息解码（**零拷贝**）：`buf` 是传输层已读入的完整消息，
    /// 载荷用 [`Bytes::slice`] 共享底层内存，不复制。
    ///
    /// 仅校验帧头与长度；`buf` 尾部多余字节被忽略（不进入载荷）。
    pub fn from_bytes_owned(buf: Bytes) -> Result<Self, FrameError> {
        let header = FrameHeader::decode(&buf)?;
        let total = HEADER_LEN + header.len as usize;
        if buf.len() < total {
            return Err(FrameError::TooShort(buf.len()));
        }
        Ok(Frame {
            header,
            payload: buf.slice(HEADER_LEN..total),
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
            seq: 42,
            frag_idx: 1,
            frag_cnt: 3,
            len: 5678,
        };
        let buf = h.encode();
        assert_eq!(buf.len(), HEADER_LEN);
        assert_eq!(&buf[0..4], MAGIC);
        let h2 = FrameHeader::decode(&buf).unwrap();
        assert_eq!(h, h2);
        assert!(h2.is_keyframe());
        assert!(h2.is_start());
        assert!(!h2.is_end());
        assert!(!h2.is_whole());
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
    fn seq_frame_roundtrip() {
        let f = Frame::with_seq(
            TRACK_VIDEO,
            CODEC_H264,
            FLAG_KEYFRAME,
            100,
            7,
            vec![9u8; 16],
        );
        let f2 = Frame::from_bytes(&f.to_bytes()).unwrap();
        assert_eq!(f2.header.seq, 7);
        assert_eq!(f2.header.pts_ms, 100);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut buf = Frame::new(TRACK_VIDEO, CODEC_H264, 0, 0, vec![])
            .to_bytes()
            .to_vec();
        buf[0] = b'X';
        assert!(Frame::from_bytes(&buf).is_err());
    }

    #[test]
    fn rejects_short_buffer() {
        assert!(Frame::from_bytes(&[0u8; 4]).is_err());
    }

    /// 确定性伪随机（xorshift64*）：任意字节不应 panic，只返回 Ok/Err。
    #[test]
    fn never_panics_on_random_bytes() {
        let mut x = 0x0123_4567_89ab_cdefu64;
        let mut next = move || {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            x = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
            x
        };
        let mut ok = 0usize;
        for _ in 0..20_000 {
            let len = (next() as usize) % 512;
            let mut buf = vec![0u8; len];
            for b in buf.iter_mut() {
                *b = next() as u8;
            }
            // 合法魔数 + 版本；长度字段取一个放得进缓冲的值（其余字段仍随机）
            if len >= HEADER_LEN {
                buf[0..4].copy_from_slice(MAGIC);
                buf[4] = VERSION;
                let payload = (next() as usize) % (len - HEADER_LEN + 1);
                buf[18..22].copy_from_slice(&(payload as u32).to_le_bytes());
            }
            if let Ok(f) = Frame::from_bytes(&buf) {
                ok += 1;
                assert_eq!(f.header.len as usize, f.payload.len());
                assert_eq!(f.header.len as usize + HEADER_LEN, f.to_bytes().len());
            }
        }
        assert!(ok > 0, "合法头应能被解析");
    }
}
