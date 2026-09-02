//! AAC ADTS 帧解析：从字节流切分出完整的 ADTS 帧。
//!
//! ffmpeg 以 `-f adts` 输出 ADTS 封装（每帧自带头，含采样率与声道信息），
//! 观看端（jmuxer）可直接从 ADTS 头提取 AudioSpecificConfig。

/// ADTS 固定头最小长度（无 CRC 时 7 字节，有 CRC 时 9 字节）。
pub const ADTS_MIN_HEADER: usize = 7;

/// ADTS 流中未切出帧的缓冲上限（1 MiB），防止异常流导致内存无限增长。
const MAX_PENDING_ADTS: usize = 1024 * 1024;

/// 判断缓冲区开头是否像 ADTS 帧（同步字 0xFFF）。
pub const fn is_adts_frame(buf: &[u8]) -> bool {
    buf.len() >= 2 && buf[0] == 0xFF && (buf[1] & 0xF0) == 0xF0
}

/// 从 ADTS 头解析帧总长度（含头）。
pub const fn adts_frame_len(buf: &[u8]) -> Option<usize> {
    if !is_adts_frame(buf) || buf.len() < ADTS_MIN_HEADER {
        return None;
    }
    let len = (((buf[3] & 0x03) as usize) << 11)
        | ((buf[4] as usize) << 3)
        | (((buf[5] >> 5) & 0x07) as usize);
    if len < ADTS_MIN_HEADER {
        None
    } else {
        Some(len)
    }
}

/// 有状态 ADTS 切分器：喂入任意字节块，产出完整 ADTS 帧。
#[derive(Default)]
pub struct AdtsSplitter {
    buf: Vec<u8>,
}

impl AdtsSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入数据，返回切出的完整 ADTS 帧。
    pub fn feed(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::with_capacity(self.buf.len() / 512 + 1);
        let mut cursor = 0usize;
        while cursor < self.buf.len() {
            let slice = &self.buf[cursor..];
            if !is_adts_frame(slice) {
                if let Some(rel_pos) = find_sync(slice) {
                    cursor += rel_pos;
                } else {
                    // 没有找到同步字，若末尾字节为 0xFF 则保留最后 1 字节等待后续字节，否则丢弃全部
                    if self.buf.last() == Some(&0xFF) {
                        cursor = self.buf.len() - 1;
                    } else {
                        cursor = self.buf.len();
                    }
                    break;
                }
                continue;
            }
            match adts_frame_len(&self.buf[cursor..]) {
                Some(len) if self.buf.len() - cursor >= len => {
                    let frame = self.buf[cursor..cursor + len].to_vec();
                    cursor += len;
                    out.push(frame);
                }
                Some(_) => break, // 帧不完整，保留从 cursor 开始的数据等待更多输入
                None => {
                    // 伪同步头（长度字段非法 < 7）：跳过 1 字节继续找同步字
                    cursor += 1;
                    continue;
                }
            }
        }
        if cursor == 0 && self.buf.len() > MAX_PENDING_ADTS {
            tracing::warn!("ADTS 流长时间未对齐，丢弃 {} 字节重新同步", self.buf.len());
            self.buf.clear();
            return out;
        }
        if cursor > 0 {
            self.buf.drain(..cursor);
        }
        out
    }

    /// 冲刷剩余的完整帧。
    pub fn finish(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut cursor = 0usize;
        while cursor < self.buf.len() && is_adts_frame(&self.buf[cursor..]) {
            match adts_frame_len(&self.buf[cursor..]) {
                Some(len) if self.buf.len() - cursor >= len => {
                    let frame = self.buf[cursor..cursor + len].to_vec();
                    cursor += len;
                    out.push(frame);
                }
                _ => break,
            }
        }
        self.buf.clear();
        out
    }
}

fn find_sync(buf: &[u8]) -> Option<usize> {
    let mut offset = 0usize;
    while offset + 1 < buf.len() {
        let rel = memchr::memchr(0xFF, &buf[offset..])?;
        let pos = offset + rel;
        if pos + 1 < buf.len() {
            if (buf[pos + 1] & 0xF0) == 0xF0 {
                return Some(pos);
            }
            offset = pos + 1;
        } else {
            break;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个最小合法的 ADTS 帧头（7 字节）+ 载荷。
    fn fake_adts(payload_len: usize) -> Vec<u8> {
        let total = ADTS_MIN_HEADER + payload_len;
        let mut h = [0u8; 7];
        h[0] = 0xFF;
        h[1] = 0xF1; // MPEG-4, layer 0, no CRC
        h[2] = 0x50; // profile AAC-LC (01), 采样率 48000 索引 3, private 0, channel 2 (0010)
        h[3] = ((total >> 11) & 0x03) as u8;
        h[4] = ((total >> 3) & 0xFF) as u8;
        h[5] = (((total & 0x07) as u8) << 5) | 0x1F;
        h[6] = 0xFC;
        let mut v = h.to_vec();
        v.extend(std::iter::repeat_n(0u8, payload_len));
        v
    }

    #[test]
    fn header_length_parse() {
        let frame = fake_adts(50);
        assert_eq!(adts_frame_len(&frame), Some(57));
        assert!(is_adts_frame(&frame));
        assert!(!is_adts_frame(&[0x00, 0x00]));
    }

    #[test]
    fn split_across_boundaries_and_resync() {
        let mut s = AdtsSplitter::new();
        let f1 = fake_adts(10);
        let f2 = fake_adts(20);
        let mut stream = Vec::new();
        stream.extend_from_slice(&[0xAA, 0xBB]); // 干扰字节（模拟丢包/错位）
        stream.extend_from_slice(&f1);
        stream.extend_from_slice(&f2);

        let mut out = Vec::new();
        for chunk in stream.chunks(9) {
            out.extend(s.feed(chunk));
        }
        out.extend(s.finish());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], f1);
        assert_eq!(out[1], f2);
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

        fn fill(&mut self, buf: &mut [u8]) {
            for b in buf.iter_mut() {
                *b = self.next() as u8;
            }
        }
    }

    /// 随机字节不应 panic，且内部缓冲受 13 位长度字段约束（≤ 8191 + 余量）。
    #[test]
    fn splitter_bounded_on_random_bytes() {
        let mut rng = Rng(0xabcd_ef01_2345_6789);
        let mut s = AdtsSplitter::new();
        let mut chunk = [0u8; 1024];
        for _ in 0..10_000 {
            rng.fill(&mut chunk);
            let _ = s.feed(&chunk);
            assert!(
                s.buf.len() <= 8191 + 64,
                "ADTS 缓冲应受 13 位长度字段约束: {}",
                s.buf.len()
            );
        }
        let _ = s.finish();
    }
}
