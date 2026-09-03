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

// ---------------------------------------------------------------------------
// v2 紧凑帧头（通信模式 v2 Phase C「字段简化」，docs/comm-mode-v2.md §2/§5）
// ---------------------------------------------------------------------------

/// v2 紧凑帧头（QUIC 复用连接上的媒体流专用）。
///
/// ```text
/// +-------+-------+---------+---------+---------+
/// | flags | track | pts_ms  | seq     | len     |
/// |  u8   |  u8   | u32 LE  | u32 LE  | u32 LE  |
/// +-------+-------+---------+---------+---------+
/// | 1     | 1     | 4       | 4       | 4       |
/// ```
///
/// 相比 v1 24 字节头的裁字段：
/// * **codec 移到协商结果**（OpenStream 声明，接收侧按 track 路由即可，
///   编解码自流内容嗅探——SPS/PPS / ADTS 头）；
/// * **magic/version 去掉**（QUIC 复用连接 + 长度前缀已提供上下文与分帧；
///   开发期允许破坏性更新，全端同步演进）；
/// * **frag 分片字段去掉**（QUIC 整帧发送，无单消息大小限制，与 WS 一致）。
///
/// 每包只带 `track + flags + pts/seq + len`（14 字节）；流身份（语义 id）由
/// QUIC stream 承载（stream 即类型，短 id 映射见 docs/comm-mode-v2.md §6）。
/// v1 24 字节头保留给 WS/SRT 单流连接（单流回退路径不受影响）。
pub const HEADER2_LEN: usize = 14;

/// v2 紧凑帧头（track/flags/pts/seq/len；codec 由协商声明，无 magic/version）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader2 {
    pub track: u8,
    pub flags: u8,
    pub pts_ms: u32,
    pub seq: u32,
    pub len: u32,
}

impl FrameHeader2 {
    /// 编码为 14 字节（track, flags, pts, seq, len 小端）。
    pub fn encode(&self) -> [u8; HEADER2_LEN] {
        let mut buf = [0u8; HEADER2_LEN];
        buf[0] = self.track;
        buf[1] = self.flags;
        buf[2..6].copy_from_slice(&self.pts_ms.to_le_bytes());
        buf[6..10].copy_from_slice(&self.seq.to_le_bytes());
        buf[10..14].copy_from_slice(&self.len.to_le_bytes());
        buf
    }

    /// 从缓冲区解析（长度不足返回 [`FrameError::TooShort`]）。
    pub fn decode(buf: &[u8]) -> Result<Self, FrameError> {
        if buf.len() < HEADER2_LEN {
            return Err(FrameError::TooShort(buf.len()));
        }
        Ok(Self {
            track: buf[0],
            flags: buf[1],
            pts_ms: u32::from_le_bytes(buf[2..6].try_into().unwrap()),
            seq: u32::from_le_bytes(buf[6..10].try_into().unwrap()),
            len: u32::from_le_bytes(buf[10..14].try_into().unwrap()),
        })
    }
}

/// v2 紧凑帧（头 + 载荷）；codec 置 0（由协商声明，消费侧不读）。
#[derive(Debug, Clone)]
pub struct Frame2 {
    pub header: FrameHeader2,
    pub payload: Bytes,
}

impl Frame2 {
    /// 从 v1 [`Frame`] 构造 v2 紧凑帧（只保留 track/flags/pts/seq/len）。
    pub fn from_frame(f: &Frame) -> Self {
        Self {
            header: FrameHeader2 {
                track: f.header.track,
                flags: f.header.flags,
                pts_ms: f.header.pts_ms,
                seq: f.header.seq,
                len: f.payload.len() as u32,
            },
            payload: f.payload.clone(),
        }
    }

    /// 编码为完整线上消息（头 + 载荷）。
    pub fn to_bytes(&self) -> Bytes {
        let mut out = Vec::with_capacity(HEADER2_LEN + self.payload.len());
        out.extend_from_slice(&self.header.encode());
        out.extend_from_slice(&self.payload);
        out.into()
    }

    /// **零额外拷贝**地构造完整线上消息（header 借用 + 载荷所有权移交）。
    ///
    /// 相比 `from_frame(&f).to_bytes()`（先 `from_frame` 克隆载荷、再 `to_bytes`
    /// 二次拷贝），本方法只做一次拷贝：把 [`FrameHeader2::encode`] 的 14 字节
    /// 头与载荷拼进新缓冲。QUIC 发送热路径（`write_msg` 前）请用它减少每帧
    /// 载荷的一整轮复制。
    pub fn to_bytes_owned(header: &FrameHeader2, payload: Bytes) -> Bytes {
        let mut out = bytes::BytesMut::with_capacity(HEADER2_LEN + payload.len());
        out.extend_from_slice(&header.encode());
        out.extend_from_slice(&payload);
        out.freeze()
    }

    /// 从线上消息解码为 v1 [`Frame`]（codec=0，消费侧不读；track/flags 保留）。
    ///
    /// **len 一致性校验**：要求头声明 `len` 与缓冲 实际载荷字节数严格相等
    /// （`buf.len() - HEADER2_LEN`），拒绝被截断或尾部多余字节的帧——避免
    /// 半截关键帧 / 配置帧流入解码器。
    pub fn to_frame(buf: &[u8]) -> Result<Frame, FrameError> {
        let header = FrameHeader2::decode(buf)?;
        let total = HEADER2_LEN + header.len as usize;
        if buf.len() < total {
            return Err(FrameError::TooShort(buf.len()));
        }
        if buf.len() != total {
            return Err(FrameError::LenMismatch {
                declared: header.len as usize,
                actual: buf.len() - HEADER2_LEN,
            });
        }
        Ok(Frame {
            header: FrameHeader {
                track: header.track,
                codec: 0,
                flags: header.flags,
                pts_ms: header.pts_ms,
                seq: header.seq,
                frag_idx: 0,
                frag_cnt: 0,
                len: header.len,
            },
            payload: Bytes::copy_from_slice(&buf[HEADER2_LEN..total]),
        })
    }

    /// 零拷贝解码（`buf` 已读入的完整消息；载荷共享底层内存）。
    ///
    /// **len 一致性校验**：与 [`Frame2::to_frame`] 相同——头声明 `len` 必须等于
    /// 缓冲实际载荷字节数，拒绝截断或携带尾部的帧。
    pub fn to_frame_owned(buf: Bytes) -> Result<Frame, FrameError> {
        let header = FrameHeader2::decode(&buf)?;
        let total = HEADER2_LEN + header.len as usize;
        if buf.len() < total {
            return Err(FrameError::TooShort(buf.len()));
        }
        if buf.len() != total {
            return Err(FrameError::LenMismatch {
                declared: header.len as usize,
                actual: buf.len() - HEADER2_LEN,
            });
        }
        Ok(Frame {
            header: FrameHeader {
                track: header.track,
                codec: 0,
                flags: header.flags,
                pts_ms: header.pts_ms,
                seq: header.seq,
                frag_idx: 0,
                frag_cnt: 0,
                len: header.len,
            },
            payload: buf.slice(HEADER2_LEN..total),
        })
    }
}

// ---- track ----
pub const TRACK_VIDEO: u8 = 0;
pub const TRACK_AUDIO: u8 = 1;
/// 文件传输轨（端点框架文件端点，docs/endpoint-model-v2.md §3；Lossless 路径）。
/// 中继对非视频轨不做关键帧门控/补发，逐帧直通——文件轨必须等观看者接入
/// 才开始推（公开方按 `/api/streams` 观看数驱动）。
pub const TRACK_FILE: u8 = 2;
/// 节点对等通道轨（即时消息/双向文件互传专用）。
pub const TRACK_CHANNEL: u8 = 3;

// ---- codec ----
pub const CODEC_H264: u8 = 1;
pub const CODEC_AAC: u8 = 2;
/// 文件轨编解码占位（无编解码语义；`TRACK_FILE` 帧专用）。
pub const CODEC_FILE: u8 = 3;
/// 通道数据编解码占位（无媒体编解码语义；`TRACK_CHANNEL` 帧专用）。
pub const CODEC_CHANNEL: u8 = 4;

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
    #[error(
        "帧头 len 与载荷不一致：头声明 {declared} 字节，实际载荷 {actual} 字节 \
         （拒绝被截断 / 携带尾部的帧，避免解码器收到半截关键帧）"
    )]
    LenMismatch { declared: usize, actual: usize },
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
        Ok(Self {
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

    pub const fn is_keyframe(&self) -> bool {
        self.flags & FLAG_KEYFRAME != 0
    }
    pub const fn is_config(&self) -> bool {
        self.flags & FLAG_CONFIG != 0
    }
    pub const fn is_start(&self) -> bool {
        self.flags & FLAG_START != 0
    }
    pub const fn is_end(&self) -> bool {
        self.flags & FLAG_END != 0
    }
    /// 是否未分片（`frag_cnt == 0`）。
    pub const fn is_whole(&self) -> bool {
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
        Self {
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
        Self {
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

    /// 从线上消息解码。**len 一致性校验**：头声明 `len` 必须等于输入载荷字节数
    /// （`buf.len() - HEADER_LEN`），拒绝截断或携带尾部的帧。
    ///
    /// 拷贝语义：输入是借用切片，载荷会 `copy_from_slice` 复制一份。
    /// 热路径（WS/QUIC 接收）请用 [`Frame::from_bytes_owned`] 避免每帧全量拷贝。
    pub fn from_bytes(buf: &[u8]) -> Result<Self, FrameError> {
        let header = FrameHeader::decode(buf)?;
        let total = HEADER_LEN + header.len as usize;
        if buf.len() < total {
            return Err(FrameError::TooShort(buf.len()));
        }
        if buf.len() != total {
            return Err(FrameError::LenMismatch {
                declared: header.len as usize,
                actual: buf.len() - HEADER_LEN,
            });
        }
        Ok(Self {
            header,
            payload: Bytes::copy_from_slice(&buf[HEADER_LEN..total]),
        })
    }

    /// 从线上消息解码（**零拷贝**）：`buf` 是传输层已读入的完整消息，
    /// 载荷用 [`Bytes::slice`] 共享底层内存，不复制。
    ///
    /// **len 一致性校验**：与 [`Frame::from_bytes`] 相同——头声明 `len` 必须等于
    /// 缓冲实际载荷字节数，拒绝截断或携带尾部的帧。
    pub fn from_bytes_owned(buf: Bytes) -> Result<Self, FrameError> {
        let header = FrameHeader::decode(&buf)?;
        let total = HEADER_LEN + header.len as usize;
        if buf.len() < total {
            return Err(FrameError::TooShort(buf.len()));
        }
        if buf.len() != total {
            return Err(FrameError::LenMismatch {
                declared: header.len as usize,
                actual: buf.len() - HEADER_LEN,
            });
        }
        Ok(Self {
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
    fn header2_roundtrip_and_field_cut() {
        // 紧凑头：14 字节（v1 24 字节裁掉 codec/magic/version/frag）
        let h2 = FrameHeader2 {
            track: TRACK_AUDIO,
            flags: FLAG_CONFIG,
            pts_ms: 99,
            seq: 7,
            len: 1234,
        };
        let buf = h2.encode();
        assert_eq!(buf.len(), HEADER2_LEN);
        assert_eq!(HEADER2_LEN, 14, "紧凑头固定 14 字节（< v1 的 24）");
        let back = FrameHeader2::decode(&buf).unwrap();
        assert_eq!(h2, back);
        assert!(FrameHeader2::decode(&buf[..10]).is_err(), "长度不足拒绝");
    }

    #[test]
    fn frame2_v1_roundtrip_preserves_track_flags_pts_seq() {
        // v1 帧 → v2 紧凑线上消息 → v1 帧：track/flags/pts/seq 保留，codec 置 0
        let f = Frame::with_seq(
            TRACK_VIDEO,
            CODEC_H264,
            FLAG_KEYFRAME,
            123,
            9,
            vec![1u8, 2, 3, 4],
        );
        let compact = Frame2::from_frame(&f);
        assert_eq!(compact.header.track, TRACK_VIDEO);
        assert_eq!(compact.header.flags, FLAG_KEYFRAME);
        assert_eq!(compact.header.pts_ms, 123);
        assert_eq!(compact.header.seq, 9);
        let wire = compact.to_bytes();
        assert_eq!(wire.len(), HEADER2_LEN + 4);
        let back = Frame2::to_frame(&wire).unwrap();
        assert_eq!(back.header.track, TRACK_VIDEO);
        assert_eq!(back.header.flags, FLAG_KEYFRAME);
        assert_eq!(back.header.pts_ms, 123);
        assert_eq!(back.header.seq, 9);
        assert_eq!(back.header.codec, 0, "codec 由协商声明，紧凑头不携带");
        assert!(back.header.is_keyframe());
        assert_eq!(back.payload.to_vec(), vec![1u8, 2, 3, 4]);
        // 零拷贝路径等价
        let back2 = Frame2::to_frame_owned(wire.clone()).unwrap();
        assert_eq!(back2.header, back.header);
        assert_eq!(back2.payload.to_vec(), back.payload.to_vec());
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

    #[test]
    fn rejects_len_mismatch_for_both_frame_versions() {
        // v1：头声明 len 与真实载荷不一致 → 拒绝（而不是静默截断/忽略尾部）。
        let good = Frame::new(TRACK_VIDEO, CODEC_H264, 0, 0, vec![9u8; 8]).to_bytes();

        // ① 尾部多余字节（头声明 len=8，实际缓冲有第 9 个字节）→ LenMismatch
        let mut trailing = good.clone().to_vec();
        trailing.push(0xAA);
        match Frame::from_bytes_owned(trailing.into()) {
            Err(FrameError::LenMismatch { declared, actual }) => {
                assert_eq!(declared, 8);
                assert_eq!(actual, 9);
            }
            other => panic!("v1 尾部多余字节应报 LenMismatch，得到 {other:?}"),
        }
        assert!(
            Frame::from_bytes(&Frame::new(TRACK_VIDEO, CODEC_H264, 0, 0, vec![9u8; 8]).to_bytes())
                .is_ok()
        );

        // ② 截断（头声明 len=8，实际缓冲只有 4 字节载荷）→ TooShort（不足则先太短）
        let mut hurt = Frame::new(TRACK_VIDEO, CODEC_H264, 0, 0, vec![9u8; 8])
            .to_bytes()
            .to_vec();
        hurt.truncate(HEADER_LEN + 4);
        assert!(Frame::from_bytes(&hurt).is_err());

        // ③ v2 紧凑帧同样校验
        let v2_good =
            Frame2::from_frame(&Frame::new(TRACK_VIDEO, CODEC_H264, 0, 0, vec![1u8; 4])).to_bytes();
        let mut v2_trailing = v2_good.clone().to_vec();
        v2_trailing.push(0xBB);
        match Frame2::to_frame_owned(v2_trailing.into()) {
            Err(FrameError::LenMismatch { declared, actual }) => {
                assert_eq!(declared, 4);
                assert_eq!(actual, 5);
            }
            other => panic!("v2 尾部多余字节应报 LenMismatch，得到 {other:?}"),
        }
        assert!(
            Frame2::to_frame_owned(v2_good.clone()).is_ok(),
            "正常 v2 帧必须通过一致性校验"
        );
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
