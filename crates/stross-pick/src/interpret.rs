//! 解读逻辑（接收侧）：pick 规则的 per-stream 解读模块
//! （docs/framework-v3.md §3.7）。
//!
//! 与装载逻辑（发送侧）两端对称：传输层负责「怎么送」
//! （[`stross_proto::message::ReliabilityProfile`]），
//! 解读模块负责「怎么读」——订阅/共享协商定稿 pick 规则后，内核按 id 装载
//! 对应解读模块，数据面建链后自治。
//!
//! * [`RealtimePacing`]：严格即时——视频/音频实时目标默认。低延迟、容忍
//!   丢帧丢块（关键帧对齐自愈）。内部复用 [`StreamChannel`]（无损直通 /
//!   有损抖动缓冲双路径，行为与既有接收链路逐项等价）；
//! * [`StrictOrdered`]：严格顺序——文件/剪贴板确定目标默认。严格有序、
//!   逐字节不丢（无损传输直通 + seq 单调校验，乱序/重复防御式丢弃）。
//!
//! [`InterpretRegistry`](crate::InterpretRegistry) 是流级容器：以强类型
//! [`StreamId`] 装载/索引解读模块，一条流一个实例——停止一条流只拆该流
//! 模块（互不级联）。
//!
//! 新契约 [`Interpreter::poll`] 单帧产出（`None` = 暂无就绪帧）：内部通道
//! 一次可能产出多帧，多出的帧暂存 `pending` 队列，逐次 poll 按序吐出——
//! 调用方循环 poll 得到的帧流与原 Vec 语义逐帧等价。

use std::collections::VecDeque;
use std::time::Instant;

use stross_proto::frame::Frame;
use stross_proto::message::PickRule;

use crate::Interpreter;
use crate::registry::{ChannelKind, StreamChannel};

/// 严格即时解读（RealtimePacing）：视频/音频实时目标默认。
///
/// 低延迟、容忍丢帧丢块（关键帧对齐自愈）。内部复用 [`StreamChannel`]：
/// 无损传输（WS/QUIC 全序不丢）直通零延迟；有损/自适应（WebRTC/SRT）经
/// 双轨抖动缓冲按序/按关键帧对齐产出——与既有接收链路逐项等价。
pub struct RealtimePacing {
    rule: PickRule,
    channel: StreamChannel,
    /// 单帧 poll 契约适配：通道一次产出多帧时暂存，逐次吐出。
    pending: VecDeque<Frame>,
}

impl RealtimePacing {
    /// 新建严格即时解读模块（`kind` 由传输可靠性契约决定，调用方计算后传入）。
    pub fn new(kind: ChannelKind) -> Self {
        Self {
            rule: PickRule::Realtime,
            channel: StreamChannel::new(kind),
            pending: VecDeque::new(),
        }
    }
}

impl Interpreter for RealtimePacing {
    fn rule(&self) -> PickRule {
        self.rule
    }

    fn push(&mut self, frame: Frame) {
        self.channel.push(frame, Instant::now());
    }

    fn poll(&mut self) -> Option<Frame> {
        if let Some(frame) = self.pending.pop_front() {
            return Some(frame);
        }
        let mut out = self.channel.poll(Instant::now()).into_iter();
        let first = out.next();
        self.pending.extend(out);
        first
    }
}

/// 严格顺序解读（StrictOrdered）：文件/剪贴板确定目标默认。
///
/// 严格有序、逐字节不丢。无损传输（QUIC/WS）保证全序不丢 → 直通队列；
/// 额外做 `seq` 单调校验（乱序/重复防御式丢弃，序列语义见协议帧头注释）。
/// 有损传输上的严格顺序（重传补齐）由传输层可靠性契约保证，本模块不缓存。
pub struct StrictOrdered {
    /// 直通队列（无损传输有序到达，按序产出）。
    queue: VecDeque<Frame>,
    /// 期望的下一 `seq`（`None` = 未开始，首帧即起点）。
    next_seq: Option<u32>,
    /// 防御性丢弃计数（乱序/重复；无损路径不应发生）。
    dropped: u64,
    /// 单帧 poll 契约适配：队列一次清空多帧时暂存，逐次吐出。
    pending: VecDeque<Frame>,
}

impl StrictOrdered {
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            next_seq: None,
            dropped: 0,
            pending: VecDeque::new(),
        }
    }

    /// 防御性丢弃计数（乱序/重复帧；无损路径不应发生，诊断用）。
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }
}

impl Default for StrictOrdered {
    fn default() -> Self {
        Self::new()
    }
}

impl Interpreter for StrictOrdered {
    fn rule(&self) -> PickRule {
        PickRule::StrictOrdered
    }

    fn push(&mut self, frame: Frame) {
        // seq 单调校验（u32 回绕安全）：非连续 = 乱序/重复 → 防御式丢弃。
        // 无损传输（QUIC/WS）seq 恒 0（协议帧头注释：无损路径取 0）——
        // 初始为 None 时保持直通；一旦检测到非 0 seq 或已初始化 next_seq，
        // 进入严格单调校验。
        match self.next_seq {
            None => {
                if frame.header.seq != 0 {
                    self.next_seq = Some(frame.header.seq.wrapping_add(1));
                }
            }
            Some(next) => {
                if frame.header.seq != next {
                    self.dropped += 1;
                    return;
                }
                self.next_seq = Some(next.wrapping_add(1));
            }
        }
        self.queue.push_back(frame);
    }

    fn poll(&mut self) -> Option<Frame> {
        if let Some(frame) = self.pending.pop_front() {
            return Some(frame);
        }
        if self.queue.is_empty() {
            return None;
        }
        let mut out = self.queue.drain(..);
        let first = out.next();
        self.pending.extend(out);
        first
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stross_proto::frame::{CODEC_H264, FLAG_KEYFRAME, Frame, TRACK_VIDEO};

    fn frame(seq: u32) -> Frame {
        Frame::with_seq(
            TRACK_VIDEO,
            CODEC_H264,
            if seq == 0 { FLAG_KEYFRAME } else { 0 },
            seq,
            seq,
            vec![0x41, 0x00, 0x01],
        )
    }

    /// 循环 poll 收集全部产出（新契约单帧产出；调用方语义 = 循环取空）。
    fn drain(i: &mut dyn Interpreter) -> Vec<Frame> {
        let mut out = Vec::new();
        while let Some(f) = i.poll() {
            out.push(f);
        }
        out
    }

    #[test]
    fn realtime_pacing_forwards_in_order() {
        let mut a = RealtimePacing::new(ChannelKind::Lossless);
        assert_eq!(a.rule(), PickRule::Realtime);
        for i in 0..3 {
            a.push(frame(i));
        }
        let out = drain(&mut a);
        assert_eq!(out.len(), 3, "无损直通按序产出");
    }

    #[test]
    fn strict_ordered_passthrough_and_seq_guard() {
        let mut a = StrictOrdered::new();
        assert_eq!(a.rule(), PickRule::StrictOrdered);
        // 无损路径 seq=0：全部直通
        for _ in 0..3 {
            a.push(frame(0));
        }
        assert_eq!(drain(&mut a).len(), 3);
        // 显式 seq：连续通过
        a.push(frame(1));
        a.push(frame(2));
        assert_eq!(drain(&mut a).len(), 2);
        // 乱序（1 → 3 缺 2）：防御式丢弃
        a.push(frame(1));
        a.push(frame(3));
        assert_eq!(drain(&mut a).len(), 1, "seq=1 通过");
        assert_eq!(a.dropped(), 1, "seq=3 乱序丢弃");
    }

    #[test]
    fn two_profiles_run_independently() {
        // 两路不同解读档同时跑：实时模块（有损抖动）与严格顺序模块互不干扰
        let mut rt = RealtimePacing::new(ChannelKind::Lossy);
        let mut so = StrictOrdered::new();
        // 实时模块：关键帧开头 + 后续帧（抖动缓冲路径）
        rt.push(frame(0));
        rt.push(frame(1));
        // 严格顺序模块：独立喂入
        so.push(frame(0));
        so.push(frame(0));
        assert_eq!(drain(&mut rt).len(), 2, "实时模块独立产出");
        assert_eq!(
            drain(&mut so).len(),
            2,
            "严格顺序模块独立产出（seq=0 直通）"
        );
    }

    #[test]
    fn empty_poll_returns_empty() {
        let mut so = StrictOrdered::new();
        assert!(so.poll().is_none());
        so.push(frame(0));
        assert!(so.poll().is_some());
        assert!(so.poll().is_none());
    }

    #[test]
    fn strict_ordered_u32_wrapping_safe() {
        let mut so = StrictOrdered::new();
        so.push(frame(u32::MAX));
        so.push(frame(0));
        so.push(frame(1));
        let out = drain(&mut so);
        assert_eq!(out.len(), 3, "u32 回绕至 0 和 1 应顺利通过");
        assert_eq!(so.dropped(), 0, "不应丢帧");
    }
}
