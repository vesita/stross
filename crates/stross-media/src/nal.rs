//! H.264 Annex-B 解析：从原始字节流切分 NAL 单元，并组装成访问单元（AU）。
//!
//! ffmpeg 以 `-f h264` 输出 Annex-B（起始码 `00 00 01` 分隔的 NAL 单元）。
//! 中继需要按"访问单元"转发，并以 IDR 判断关键帧，供观看端对齐 GOP。

/// NAL 单元类型（低 5 位）。
pub const NAL_SLICE_NON_IDR: u8 = 1;
pub const NAL_SLICE_IDR: u8 = 5;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;

/// 提取 NAL 单元类型。
pub fn nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| b & 0x1f)
}

/// 有状态 Annex-B 切分器：喂入任意长度的字节块，产出完整的 NAL 单元（不含起始码）。
///
/// 输入的字节可以按任意边界到达（管道读取天然如此）。
#[derive(Default)]
pub struct AnnexBSplitter {
    buf: Vec<u8>,
}

impl AnnexBSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入数据，返回新切出的完整 NAL 单元（不含起始码）。
    pub fn feed(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        let mut prev = 0usize; // 上一个起始码之后的位置（当前 NAL 起点）
        let mut i = 3usize;
        while i + 2 < self.buf.len() {
            if self.buf[i - 2] == 0 && self.buf[i - 1] == 0 && self.buf[i] == 1 {
                // 起始码可能为 3 字节 (00 00 01) 或 4 字节 (00 00 00 01)
                let code_start = if i >= 3 && self.buf[i - 3] == 0 { i - 3 } else { i - 2 };
                if code_start > prev {
                    out.push(self.buf[prev..code_start].to_vec());
                }
                prev = i + 1;
                i += 3;
            } else {
                i += 1;
            }
        }
        // 保留未完成的尾部，等待下一次 feed
        self.buf.drain(..prev);
        out
    }

    /// 流结束时冲刷剩余数据（返回最后一个不完整的 NAL）。
    pub fn finish(mut self) -> Vec<Vec<u8>> {
        let rest = std::mem::take(&mut self.buf);
        if rest.is_empty() {
            Vec::new()
        } else {
            vec![rest]
        }
    }
}

/// 一个访问单元：一个视频帧的所有 NAL 单元。
#[derive(Debug, Clone)]
pub struct AccessUnit {
    pub nals: Vec<Vec<u8>>,
    /// 是否包含 IDR（关键帧）。
    pub keyframe: bool,
}

impl AccessUnit {
    /// 序列化为带起始码的 Annex-B 字节流。
    pub fn to_annex_b(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in &self.nals {
            out.extend_from_slice(&[0, 0, 1]);
            out.extend_from_slice(nal);
        }
        out
    }

    /// 载荷总字节数（不含起始码）。
    pub fn payload_len(&self) -> usize {
        self.nals.iter().map(|n| n.len()).sum()
    }
}

/// 解析 slice header 的 `first_mb_in_slice`（Exp-Golomb 无符号整数）。
///
/// 它是 slice 头部的第一个码字：`0` 表示本 slice 是**一帧的第一个 slice**。
/// 多 slice 编码（slice 线程）会输出 `[slice1, slice2, …]`，必须据此分组。
///
/// 返回 `None` 表示无法解析（调用方应保守地视为新帧开头）。
fn first_mb_in_slice(nal: &[u8]) -> Option<u64> {
    if nal.len() < 2 {
        return None;
    }
    // 去除 NAL header 字节后的防竞争字节（00 00 03 → 00 00），取前 6 字节足够
    let mut cleaned = [0u8; 6];
    let mut ci = 0usize;
    let mut zeros = 0u8;
    let mut si = 1usize;
    while ci < cleaned.len() && si < nal.len() {
        let b = nal[si];
        si += 1;
        if b == 0 {
            zeros += 1;
        } else if b == 3 && zeros >= 2 {
            zeros = 0; // 防竞争字节，丢弃
            continue;
        } else {
            zeros = 0;
        }
        cleaned[ci] = b;
        ci += 1;
    }
    if ci == 0 {
        return None;
    }
    // MSB 优先位读取
    let bit = |p: usize| -> Option<u8> {
        let byte = p / 8;
        if byte >= ci {
            return None;
        }
        Some((cleaned[byte] >> (7 - (p % 8))) & 1)
    };
    // Exp-Golomb：m 个前导零 + 1 + m 位数值
    let mut p = 0usize;
    let mut m = 0u32;
    while bit(p)? == 0 {
        m += 1;
        p += 1;
        if m > 32 {
            return None;
        }
    }
    p += 1; // 跳过 1
    let mut value = 0u64;
    for _ in 0..m {
        value = (value << 1) | bit(p)? as u64;
        p += 1;
    }
    Some((1u64 << m) - 1 + value)
}

/// 访问单元组装器：按 **帧** 拆分组装，SPS/PPS/SEI 挂在随后的首 slice 上。
///
/// ffmpeg（`repeat-headers=1`）输出形如 `SPS PPS SEI IDR P P P …`，
/// 组装结果：
///
/// * 关键帧：`[SPS, PPS, SEI?, IDR-slice…]`
/// * 普通帧：`[slice…]`
///
/// 支持多 slice 编码（同一帧的多个 slice 归入同一访问单元）。
#[derive(Default)]
pub struct AccessUnitBuilder {
    /// 自上一个帧以来收集的配置 NAL（SPS/PPS/SEI）。
    pending: Vec<Vec<u8>>,
    /// 当前帧已收集的 slice。
    current: Vec<Vec<u8>>,
    /// 当前帧是否含 IDR。
    keyframe: bool,
}

impl AccessUnitBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 推入一个 NAL；遇到新的一帧（`first_mb_in_slice == 0`）时产出上一帧。
    pub fn push(&mut self, nal: Vec<u8>) -> Option<AccessUnit> {
        match nal_type(&nal) {
            Some(NAL_SLICE_NON_IDR) | Some(NAL_SLICE_IDR) => {
                let is_idr = nal_type(&nal) == Some(NAL_SLICE_IDR);
                let first_slice = first_mb_in_slice(&nal).is_none_or(|v| v == 0);
                if first_slice && !self.current.is_empty() {
                    let au = self.take();
                    self.current.push(nal);
                    self.keyframe = is_idr;
                    return Some(au);
                }
                self.current.push(nal);
                self.keyframe = is_idr;
                None
            }
            // 配置 / SEI 等 → 暂存，等 slice 到来
            _ => {
                // 防呆：流里一直没有 slice 时，避免 pending 无限增长
                if self.pending.len() >= 32 {
                    self.pending.clear();
                }
                self.pending.push(nal);
                None
            }
        }
    }

    /// 冲刷最后一个帧。
    pub fn finish(&mut self) -> Option<AccessUnit> {
        if self.current.is_empty() && self.pending.is_empty() {
            None
        } else {
            Some(self.take())
        }
    }

    fn take(&mut self) -> AccessUnit {
        let mut nals = std::mem::take(&mut self.pending);
        nals.append(&mut std::mem::take(&mut self.current));
        AccessUnit {
            nals,
            keyframe: std::mem::take(&mut self.keyframe),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nal(kind: u8, payload: u8) -> Vec<u8> {
        let mut v = vec![kind];
        v.extend(std::iter::repeat_n(payload, 10));
        v
    }

    #[test]
    fn split_across_chunk_boundaries() {
        let mut s = AnnexBSplitter::new();
        let a = nal(NAL_SPS, 1);
        let b = nal(NAL_SLICE_NON_IDR, 2);
        let mut stream = Vec::new();
        stream.extend_from_slice(&[0, 0, 0, 1]);
        stream.extend_from_slice(&a);
        stream.extend_from_slice(&[0, 0, 1]);
        stream.extend_from_slice(&b);

        // 以奇怪的边界喂入
        let mut out = Vec::new();
        for chunk in stream.chunks(3) {
            out.extend(s.feed(chunk));
        }
        out.extend(s.finish());
        assert_eq!(out.len(), 2);
        assert_eq!(nal_type(&out[0]), Some(NAL_SPS));
        assert_eq!(out[0], a);
        assert_eq!(out[1], b);
    }

    #[test]
    fn access_unit_boundaries_and_keyframe() {
        let mut b = AccessUnitBuilder::new();
        // 重复头部流：SPS PPS IDR → 关键帧 [SPS,PPS,IDR]
        assert!(b.push(nal(NAL_SPS, 0)).is_none());
        assert!(b.push(nal(NAL_PPS, 0)).is_none());
        assert!(b.push(nal(NAL_SLICE_IDR, 0)).is_none(), "首帧不产出");
        // P 帧到来 → 产出关键帧
        let kf = b.push(nal(NAL_SLICE_NON_IDR, 0)).expect("P 帧应切出关键帧");
        assert!(kf.keyframe);
        assert_eq!(kf.nals.len(), 3);
        // 下一个 P 帧 → 产出普通帧
        let p = b.push(nal(NAL_SLICE_NON_IDR, 0)).unwrap();
        assert!(!p.keyframe);
        assert_eq!(p.nals.len(), 1);
        // 第二个 GOP：SPS IDR P（IDR 到来会先切出上一帧，SPS 随上一帧走）
        assert!(b.push(nal(NAL_SPS, 0)).is_none());
        let prev = b.push(nal(NAL_SLICE_IDR, 0)).expect("IDR 应先切出上一帧");
        assert!(!prev.keyframe);
        assert_eq!(prev.nals.len(), 2); // [SPS, P]
        let kf2 = b.push(nal(NAL_SLICE_NON_IDR, 0)).unwrap();
        assert!(kf2.keyframe);
        assert_eq!(kf2.nals.len(), 1); // [IDR]
        let p2 = b.push(nal(NAL_SLICE_NON_IDR, 0)).unwrap();
        assert!(!p2.keyframe);
        let last = b.finish().expect("finish 应切出最后一帧");
        assert!(!last.keyframe);
    }

    #[test]
    fn multi_slice_frames_grouped() {
        let mut b = AccessUnitBuilder::new();
        // 第一帧：first_mb=0 的 IDR slice（0x80）+ first_mb=1 的 slice（0x40）
        let mut s1 = vec![NAL_SLICE_IDR, 0x80];
        s1.extend([0u8; 8]);
        let mut s2 = vec![NAL_SLICE_IDR, 0x40];
        s2.extend([0u8; 8]);
        assert!(b.push(s1).is_none());
        assert!(b.push(s2).is_none(), "同一帧的 slice 不应切分");
        // 下一帧到来 → 产出含两个 slice 的关键帧
        let au = b.push(nal(NAL_SLICE_NON_IDR, 0)).unwrap();
        assert!(au.keyframe);
        assert_eq!(au.nals.len(), 2);
    }

    #[test]
    fn access_unit_without_repeated_headers() {
        let mut b = AccessUnitBuilder::new();
        // 无重复头部：IDR P P IDR P
        assert!(b.push(nal(NAL_SLICE_IDR, 0)).is_none());
        let kf = b.push(nal(NAL_SLICE_NON_IDR, 0)).unwrap();
        assert!(kf.keyframe);
        assert_eq!(kf.nals.len(), 1);
        assert!(b.push(nal(NAL_SLICE_NON_IDR, 0)).is_some());
        assert!(b.push(nal(NAL_SLICE_IDR, 0)).is_some(), "IDR 应先切出上一帧");
        let kf2 = b.push(nal(NAL_SLICE_NON_IDR, 0)).unwrap();
        assert!(kf2.keyframe);
        let last = b.finish().unwrap();
        assert!(!last.keyframe);
    }

    #[test]
    fn pending_config_capped() {
        let mut b = AccessUnitBuilder::new();
        for _ in 0..100 {
            assert!(b.push(nal(NAL_SPS, 0)).is_none());
        }
        assert!(b.push(nal(NAL_SLICE_IDR, 0)).is_none());
        let kf = b.push(nal(NAL_SLICE_NON_IDR, 0)).unwrap();
        assert!(kf.keyframe);
        assert!(kf.nals.len() <= 33, "pending 应被截断，避免内存膨胀: {}", kf.nals.len());
    }

    #[test]
    fn annex_b_serialization() {
        let mut b = AccessUnitBuilder::new();
        assert!(b.push(nal(NAL_SPS, 0)).is_none());
        assert!(b.push(nal(NAL_SLICE_IDR, 0)).is_none());
        let au = b.finish().expect("finish 应组装出访问单元");
        assert_eq!(au.nals.len(), 2);
        let bytes = au.to_annex_b();
        assert_eq!(&bytes[..3], &[0, 0, 1]);
        assert_eq!(bytes.len(), 3 + 11 + 3 + 11);
    }

    #[test]
    fn first_mb_exp_golomb() {
        // first_mb=0 → "1" → 0x80；first_mb=1 → "010" → 0x40
        let mut s0 = vec![NAL_SLICE_NON_IDR, 0x80];
        s0.extend([0u8; 8]);
        let mut s1 = vec![NAL_SLICE_NON_IDR, 0x40];
        s1.extend([0u8; 8]);
        assert_eq!(first_mb_in_slice(&s0), Some(0));
        assert_eq!(first_mb_in_slice(&s1), Some(1));
    }
}
