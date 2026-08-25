//! H.264 Annex-B 解析：从原始字节流切分 NAL 单元，并组装成访问单元（AU）。
//!
//! ffmpeg 以 `-f h264` 输出 Annex-B（起始码 `00 00 01` 分隔的 NAL 单元）。
//! 中继需要按"访问单元"转发，并以 IDR 判断关键帧，供观看端对齐 GOP。

/// NAL 单元类型（低 5 位）。
pub const NAL_SLICE_NON_IDR: u8 = 1;
pub const NAL_SLICE_IDR: u8 = 5;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;

/// Annex-B 流里单个段（两个起始码之间）的上限。
///
/// 超过即视为失同步：丢弃该段/缓冲重新同步，防止垃圾流（无起始码或伪起始码）
/// 让内部缓冲无限增长。8 MiB 足够容纳正常编码的高码率关键帧
/// （1080p@6Mbps、GOP 2s 的关键帧最坏约 1.5 MiB）。
const MAX_PENDING_NAL: usize = 8 * 1024 * 1024;

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
        // 流开头就是 3 字节起始码（00 00 01）：主循环从 i=3 起扫，
        // 只能命中"结束于 ≥3 位置"的码；开头的码需在此先行识别。
        // （4 字节码 00 00 00 01 的"1"在 buf[3]，由主循环在 i=3 命中。）
        let mut prev =
            if self.buf.len() >= 3 && self.buf[0] == 0 && self.buf[1] == 0 && self.buf[2] == 1 {
                3
            } else {
                0
            };
        let mut i = 3usize;
        while i + 2 < self.buf.len() {
            if self.buf[i - 2] == 0 && self.buf[i - 1] == 0 && self.buf[i] == 1 {
                // 起始码可能为 3 字节 (00 00 01) 或 4 字节 (00 00 00 01)
                let code_start = if i >= 3 && self.buf[i - 3] == 0 {
                    i - 3
                } else {
                    i - 2
                };
                if code_start > prev {
                    let seg = code_start - prev;
                    if seg <= MAX_PENDING_NAL {
                        out.push(self.buf[prev..code_start].to_vec());
                    } else {
                        // 单段超大（伪起始码 / 垃圾流）：丢弃该段，不产出
                        tracing::warn!("Annex-B 单段过大（{seg} 字节），已丢弃");
                    }
                }
                prev = i + 1;
                i += 3;
            } else {
                i += 1;
            }
        }
        // 防呆：长时间无起始码的可疑数据累积超过上限时整体丢弃重新同步
        if prev == 0 && self.buf.len() > MAX_PENDING_NAL {
            tracing::warn!(
                "Annex-B 流长时间无起始码，丢弃 {} 字节重新同步",
                self.buf.len()
            );
            self.buf.clear();
            return out;
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
        // 精确预分配（起始码 3 字节 × NAL 数 + 载荷总长），避免逐 NAL 扩容拷贝
        let total: usize = self.nals.iter().map(|n| n.len() + 3).sum();
        let mut out = Vec::with_capacity(total);
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

/// 从 SPS NAL（不含起始码）解析图像宽高（含 `frame_cropping` 裁剪）。
///
/// rawvideo 解码输出需要"每帧字节数 = 宽 × 高 × 像素字节"，而编码分辨率
/// 不随协议帧头传递，只能解码 SPS 得知——桌面播放后端
/// （[`crate::playback::FfmpegPlaybackSink`]）依赖本函数确定帧大小。
///
/// 返回 `None` 表示无法解析（非法 / 截断的 SPS），调用方应保守处理。
pub fn sps_dimensions(nal: &[u8]) -> Option<(u32, u32)> {
    if nal_type(nal) != Some(NAL_SPS) || nal.len() < 4 {
        return None;
    }
    let rbsp = de_emulation_prevention(&nal[1..]);
    let mut bits = BitReader::new(&rbsp);

    let profile_idc = bits.u(8)?;
    bits.skip(8)?; // constraint_set0..5_flag + reserved_zero_2bits
    bits.skip(8)?; // level_idc
    bits.ue()?; // seq_parameter_set_id

    // 高档次才显式携带色度格式；baseline/main 隐含 4:2:0（chroma_format_idc = 1）
    let mut chroma_format_idc: u32 = 1;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = bits.ue()?;
        if chroma_format_idc == 3 {
            bits.skip(1)?; // separate_colour_plane_flag
        }
        bits.ue()?; // bit_depth_luma_minus8
        bits.ue()?; // bit_depth_chroma_minus8
        bits.skip(1)?; // qpprime_y_zero_transform_bypass_flag
        if bits.bit()? == 1 {
            // seq_scaling_matrix_present_flag：跳过缩放矩阵（解码不需要其内容）
            let count = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..count {
                if bits.bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let mut last = 8i32;
                    let mut next = 8i32;
                    for _ in 0..size {
                        if next != 0 {
                            let delta = bits.se()?;
                            next = (last + delta + 256) % 256;
                        }
                        last = if next == 0 { last } else { next };
                    }
                }
            }
        }
    }

    bits.ue()?; // log2_max_frame_num_minus4
    match bits.ue()? {
        0 => {
            bits.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            bits.skip(1)?; // delta_pic_order_always_zero_flag
            bits.se()?; // offset_for_non_ref_pic
            bits.se()?; // offset_for_top_to_bottom_field
            let n = bits.ue()?;
            for _ in 0..n {
                bits.se()?; // offset_for_ref_frame[i]
            }
        }
        _ => {}
    }
    bits.ue()?; // max_num_ref_frames
    bits.skip(1)?; // gaps_in_frame_num_value_allowed_flag

    let pic_width_in_mbs_minus1 = bits.ue()?;
    let pic_height_in_map_units_minus1 = bits.ue()?;
    let frame_mbs_only_flag = bits.bit()?;
    if frame_mbs_only_flag == 0 {
        bits.skip(1)?; // mb_adaptive_frame_field_flag
    }
    bits.skip(1)?; // direct_8x8_inference_flag

    let mut crop = [0u32; 4]; // left, right, top, bottom
    if bits.bit()? == 1 {
        for c in &mut crop {
            *c = bits.ue()?;
        }
    }

    // 裁剪单位（H.264 7.4.2.1.1）：4:2:0 为 2 像素/单位
    let (crop_unit_x, crop_unit_y) = if chroma_format_idc == 0 {
        (1, 2 - frame_mbs_only_flag)
    } else {
        match chroma_format_idc {
            1 => (2, 2), // 4:2:0
            2 => (2, 1), // 4:2:2
            _ => (1, 1), // 4:4:4 及以上
        }
    };
    let width = (pic_width_in_mbs_minus1 + 1) * 16 - (crop[0] + crop[1]) * crop_unit_x;
    let height = (pic_height_in_map_units_minus1 + 1) * (2 - frame_mbs_only_flag) * 16
        - (crop[2] + crop[3]) * crop_unit_y;
    if width == 0 || height == 0 {
        return None;
    }
    Some((width, height))
}

/// 去掉防竞争字节（`00 00 03` → `00 00`），把 EBSP 转成 RBSP。
fn de_emulation_prevention(ebsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ebsp.len());
    let mut zeros = 0u8;
    for &b in ebsp {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        out.push(b);
    }
    out
}

/// 逐位读取器（MSB 优先），用于解析 H.264 的 Exp-Golomb 码字。
struct BitReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// 读 1 位。
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.buf.get(self.pos / 8)?;
        let b = (byte >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Some(b as u32)
    }

    /// 跳过 n 位。
    fn skip(&mut self, n: usize) -> Option<()> {
        if self.pos + n > self.buf.len() * 8 {
            return None;
        }
        self.pos += n;
        Some(())
    }

    /// 读 n 位（n ≤ 32）为无符号整数。
    fn u(&mut self, n: usize) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    /// 无符号 Exp-Golomb 码（ue）。
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0u32;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None; // 非法码字防呆
            }
        }
        let mut v = 0u32;
        for _ in 0..zeros {
            v = (v << 1) | self.bit()?;
        }
        Some((1u32 << zeros) - 1 + v)
    }

    /// 有符号 Exp-Golomb 码（se）。
    fn se(&mut self) -> Option<i32> {
        let ue = self.ue()?;
        let k = ue.div_ceil(2) as i32;
        Some(if ue % 2 == 1 { k } else { -k })
    }
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
    ///
    /// 配置 NAL（SPS/PPS/SEI，`pending`）归属**随后的 slice 所在帧**：新一帧的
    /// 首个 slice 到达时，先把 `pending` 并入新帧开头，再产出上一帧——
    /// 否则 SPS/PPS 会配给上一帧，关键帧变成"光杆 IDR"（无 SPS/PPS），
    /// 中途接入的观看端（含级联代理）无法解析分辨率，解码 0 帧。
    pub fn push(&mut self, nal: Vec<u8>) -> Option<AccessUnit> {
        match nal_type(&nal) {
            Some(NAL_SLICE_NON_IDR) | Some(NAL_SLICE_IDR) => {
                let is_idr = nal_type(&nal) == Some(NAL_SLICE_IDR);
                let first_slice = first_mb_in_slice(&nal).is_none_or(|v| v == 0);
                if first_slice {
                    // 产出上一帧（pending 不属于它）
                    let prev = if self.current.is_empty() {
                        None
                    } else {
                        Some(AccessUnit {
                            nals: std::mem::take(&mut self.current),
                            keyframe: std::mem::take(&mut self.keyframe),
                        })
                    };
                    // 新帧 = 待附配置（SPS/PPS/SEI）+ 本 slice
                    let mut nals = std::mem::take(&mut self.pending);
                    nals.push(nal);
                    self.current = nals;
                    self.keyframe = is_idr;
                    return prev;
                }
                // 同帧后续 slice（多 slice 编码）：追加
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

    /// 流以 3 字节起始码开头（`AccessUnit::to_annex_b` 的产物）时，
    /// 第一个 NAL 必须被正确切出，且不含起始码前缀。
    #[test]
    fn split_with_leading_three_byte_start_code() {
        let mut s = AnnexBSplitter::new();
        let sps = nal(NAL_SPS, 3);
        let pps = nal(NAL_PPS, 4);
        let mut stream = Vec::new();
        stream.extend_from_slice(&[0, 0, 1]); // 3 字节码在 offset 0
        stream.extend_from_slice(&sps);
        stream.extend_from_slice(&[0, 0, 1]);
        stream.extend_from_slice(&pps);
        let mut out = s.feed(&stream);
        out.extend(s.finish());
        assert_eq!(out.len(), 2, "开头的 3 字节码应被识别: {out:?}");
        assert_eq!(out[0], sps, "第一个 NAL 不应含起始码前缀");
        assert_eq!(out[1], pps);
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
        // 第二个 GOP：SPS IDR P（SPS 归关键帧：IDR 到达先切出上一帧，SPS 并入新帧）
        assert!(b.push(nal(NAL_SPS, 0)).is_none());
        let prev = b.push(nal(NAL_SLICE_IDR, 0)).expect("IDR 应先切出上一帧");
        assert!(!prev.keyframe);
        assert_eq!(prev.nals.len(), 1); // [P]——上一帧不带 SPS
        let kf2 = b.push(nal(NAL_SLICE_NON_IDR, 0)).unwrap();
        assert!(kf2.keyframe);
        assert_eq!(kf2.nals.len(), 2); // [SPS, IDR]——关键帧含 SPS（自愈前提）
        let p2 = b.push(nal(NAL_SLICE_NON_IDR, 0)).unwrap();
        assert!(!p2.keyframe);
        let last = b.finish().expect("finish 应切出最后一帧");
        assert!(!last.keyframe);
    }

    /// 回归：repeat_headers=1 流中**每个**关键帧必须携带 SPS/PPS。
    ///
    /// 曾修 bug：配置 NAL（pending）被配给上一帧，后续关键帧变成"光杆 IDR"，
    /// relay 缓存它转发后，中途接入的观看端（含级联代理）无法解析分辨率
    /// （`parse_sps_size` 失败），解码 0 帧。
    #[test]
    fn every_keyframe_carries_sps() {
        let mut b = AccessUnitBuilder::new();
        // GOP1：SPS PPS SEI IDR P
        assert!(b.push(nal(NAL_SPS, 0)).is_none());
        assert!(b.push(nal(NAL_PPS, 0)).is_none());
        assert!(b.push(nal(6, 0)).is_none(), "SEI 进 pending");
        assert!(b.push(nal(NAL_SLICE_IDR, 0)).is_none(), "首帧不产出");
        let kf1 = b.push(nal(NAL_SLICE_NON_IDR, 0)).expect("切出关键帧 1");
        assert!(kf1.keyframe);
        let types1: Vec<u8> = kf1.nals.iter().filter_map(|n| nal_type(n)).collect();
        assert!(
            types1.contains(&NAL_SPS) && types1.contains(&NAL_PPS),
            "关键帧 1 必须含 SPS/PPS: {types1:?}"
        );
        // 两个 P 帧
        assert!(!b.push(nal(NAL_SLICE_NON_IDR, 0)).unwrap().keyframe);
        assert!(!b.push(nal(NAL_SLICE_NON_IDR, 0)).unwrap().keyframe);
        // GOP2：SPS PPS IDR P（IDR 先切出上一 P 帧，SPS 并入关键帧）
        assert!(b.push(nal(NAL_SPS, 0)).is_none());
        assert!(b.push(nal(NAL_PPS, 0)).is_none());
        let prev = b.push(nal(NAL_SLICE_IDR, 0)).expect("IDR 切出上一帧");
        assert!(!prev.keyframe);
        let kf2 = b.push(nal(NAL_SLICE_NON_IDR, 0)).expect("切出关键帧 2");
        assert!(kf2.keyframe);
        let types2: Vec<u8> = kf2.nals.iter().filter_map(|n| nal_type(n)).collect();
        assert!(
            types2.contains(&NAL_SPS) && types2.contains(&NAL_PPS),
            "关键帧 2 必须含 SPS/PPS（级联/中途接入依赖）: {types2:?}"
        );
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
        assert!(
            b.push(nal(NAL_SLICE_IDR, 0)).is_some(),
            "IDR 应先切出上一帧"
        );
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
        assert!(
            kf.nals.len() <= 33,
            "pending 应被截断，避免内存膨胀: {}",
            kf.nals.len()
        );
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

    /// 确定性伪随机数（xorshift64*），保证测试可复现。
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }

    /// 纯垃圾（无任何起始码）不应 panic：超过上限后整体丢弃重新同步，
    /// 且重同步后仍能正常切分真实流。
    #[test]
    fn splitter_bounded_and_resyncs_on_garbage() {
        let mut s = AnnexBSplitter::new();
        // 全零字节不含起始码（00 00 01），是"无起始码"的最坏情况
        let garbage = vec![0u8; MAX_PENDING_NAL + 64];
        let out = s.feed(&garbage);
        assert!(out.is_empty(), "垃圾不应产出 NAL");
        assert!(
            s.buf.len() <= MAX_PENDING_NAL,
            "无起始码时内部缓冲应被截断: {}",
            s.buf.len()
        );
        // 重同步后：真实流仍能正常切分（feed 切中间段，finish 冲刷末尾段）
        let mut stream = Vec::new();
        stream.extend_from_slice(&[0, 0, 0, 1]); // 4 字节起始码（扫描可识别）
        stream.extend_from_slice(&nal(NAL_SPS, 7));
        stream.extend_from_slice(&[0, 0, 1]);
        stream.extend_from_slice(&nal(NAL_SLICE_NON_IDR, 8));
        let out = s.feed(&stream);
        assert_eq!(out.len(), 1, "SPS 应作为第一段切出");
        assert_eq!(s.finish().len(), 1, "末尾 slice 应由 finish 冲刷");
    }

    /// 随机 NAL（覆盖全部类型，含非法值）不应 panic，且 pending 有界。
    #[test]
    fn au_builder_never_panics_on_random_nals() {
        let mut rng = Rng(0xdead_beef_cafe_f00d);
        let mut b = AccessUnitBuilder::new();
        let mut produced = 0usize;
        for _ in 0..50_000 {
            let kind = (rng.next() % 32) as u8; // 覆盖 0..31 全部 NAL 类型
            let len = (rng.next() % 64) as usize;
            let mut nal = vec![kind];
            for _ in 0..len {
                nal.push(rng.next() as u8);
            }
            if b.push(nal).is_some() {
                produced += 1;
            }
            assert!(
                b.pending.len() <= 32,
                "pending 应保持有界: {}",
                b.pending.len()
            );
        }
        assert!(produced > 0);
        let _ = b.finish();
    }

    /// 超大伪段（起始码之间超过上限）应被丢弃而不是产出 / 撑爆缓冲。
    #[test]
    fn oversized_segment_dropped() {
        let mut s = AnnexBSplitter::new();
        // 起始码 + 超大伪段 + 起始码 + 小段
        let mut stream = Vec::with_capacity(MAX_PENDING_NAL + 64);
        stream.extend_from_slice(&[0, 0, 1]);
        stream.resize(MAX_PENDING_NAL + 8, 0xAA); // 无起始码的巨型段
        stream.extend_from_slice(&[0, 0, 1, 0x65, 0x88]); // 正常小段
        let out = s.feed(&stream);
        assert!(
            out.iter().all(|nal| nal.len() <= MAX_PENDING_NAL),
            "不应产出超大 NAL"
        );
        assert!(s.buf.len() <= MAX_PENDING_NAL, "缓冲应保持有界");
    }

    /// 测试用逐位写入器（MSB 优先），构造合法 SPS 码流。
    struct BitWriter {
        bits: Vec<u8>,
        pos: usize,
    }

    impl BitWriter {
        fn new() -> Self {
            Self {
                bits: Vec::new(),
                pos: 0,
            }
        }
        fn bit(&mut self, b: u32) {
            if self.pos.is_multiple_of(8) {
                self.bits.push(0);
            }
            let byte = self.bits.last_mut().unwrap();
            *byte |= ((b & 1) as u8) << (7 - (self.pos % 8));
            self.pos += 1;
        }
        fn u(&mut self, v: u32, n: usize) {
            for i in (0..n).rev() {
                self.bit((v >> i) & 1);
            }
        }
        fn ue(&mut self, v: u32) {
            let m = 32 - (v + 1).leading_zeros();
            for _ in 0..(m - 1) {
                self.bit(0);
            }
            self.u(v + 1, m as usize);
        }
        fn finish(&mut self) -> Vec<u8> {
            std::mem::take(&mut self.bits)
        }
    }

    /// 构造 baseline SPS NAL（帧率无关，只带宽高与裁剪）。
    fn make_sps(width_mbs: u32, height_map_units: u32, crop_bottom: u32) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.u(66, 8); // profile_idc = baseline
        w.u(0, 8); // constraint_set 标志
        w.u(31, 8); // level_idc
        w.ue(0); // seq_parameter_set_id
        w.ue(0); // log2_max_frame_num_minus4
        w.ue(0); // pic_order_cnt_type = 0
        w.ue(0); // log2_max_pic_order_cnt_lsb_minus4
        w.ue(1); // max_num_ref_frames
        w.bit(0); // gaps_in_frame_num_value_allowed_flag
        w.ue(width_mbs - 1); // pic_width_in_mbs_minus1
        w.ue(height_map_units - 1); // pic_height_in_map_units_minus1
        w.bit(1); // frame_mbs_only_flag
        w.bit(1); // direct_8x8_inference_flag
        w.bit(u32::from(crop_bottom > 0)); // frame_cropping_flag
        if crop_bottom > 0 {
            w.ue(0); // crop_left
            w.ue(0); // crop_right
            w.ue(0); // crop_top
            w.ue(crop_bottom);
        }
        let mut rbsp = w.finish();
        rbsp.push(0x80); // rbsp_trailing_bits（stop bit）
        let mut nal = vec![0x67]; // NAL header（type 7）
        nal.extend(rbsp);
        nal
    }

    #[test]
    fn sps_dimensions_640x360() {
        // 640x360 = 40x23 宏块，底部裁剪 4 单位（×2 = 8 像素，4:2:0）
        let sps = make_sps(40, 23, 4);
        assert_eq!(sps_dimensions(&sps), Some((640, 360)));
    }

    #[test]
    fn sps_dimensions_1280x720_no_crop() {
        // 1280x720 = 80x45 宏块，无裁剪
        let sps = make_sps(80, 45, 0);
        assert_eq!(sps_dimensions(&sps), Some((1280, 720)));
    }

    #[test]
    fn sps_dimensions_rejects_non_sps_or_truncated() {
        assert_eq!(sps_dimensions(&[]), None);
        assert_eq!(sps_dimensions(&[0x67, 0x00]), None, "截断的 SPS");
        assert_eq!(sps_dimensions(&[0x65, 0x88]), None, "slice NAL 不是 SPS");
    }

    /// 真实 x264 编码（high profile level 3.0，`zerolatency` 参数）的 SPS：
    /// 640x360，含防竞争字节（`00 00 03`），验证解析器对真实码流的兼容性。
    #[test]
    fn sps_dimensions_real_x264_high_profile() {
        let sps: Vec<u8> = vec![
            0x67, 0x64, 0x00, 0x1e, 0xac, 0xb4, 0x05, 0x01, 0x7f, 0xcb, 0x80, 0x88, 0x00, 0x00,
            0x03, 0x00, 0x08, 0x00, 0x00, 0x03, 0x01, 0x84, 0x78, 0xb1, 0x75,
        ];
        assert_eq!(sps_dimensions(&sps), Some((640, 360)));
    }
}
