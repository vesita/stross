//! 解读逻辑（接收侧）：pick 规则的 per-stream 解读模块
//! （通信模式 v2，docs/comm-mode-v2.md §3.0）。
//!
//! 与 [`load`](super::load)（装载逻辑，发送侧）两端对称：传输层负责
//! 「怎么送」（[`ReliabilityProfile`]），解读模块负责「怎么读」——订阅/共享
//! 协商定稿 pick 规则后，内核按 id 装载对应解读模块，数据面建链后自治。
//!
//! * [`RealtimePacing`]：严格即时——视频/音频实时目标默认。低延迟、容忍
//!   丢帧丢块（关键帧对齐自愈）。内部复用 [`StreamChannel`]（无损直通 /
//!   有损抖动缓冲双路径，行为与既有接收链路逐项等价）；
//! * [`StrictOrdered`]：严格顺序——文件/剪贴板确定目标默认。严格有序、
//!   逐字节不丢（无损传输直通 + seq 单调校验，乱序/重复防御式丢弃）。
//!
//! [`InterpretRegistry`] 是会话级容器：以 `session_id` 装载/索引解读模块，
//! 一条流一个实例——停止一条流只拆该流模块（互不级联）。

use std::collections::VecDeque;
use std::time::Instant;

use stross_proto::frame::Frame;
use stross_proto::message::PickRule;

use super::buffer::JitterStats;
use super::manager::{ChannelKind, StreamChannel};

/// 解读逻辑（接收侧）：把传输层产出的帧「解读」成可播放/可消费的顺序。
///
/// 实现约定（与 [`StreamChannel`] 一致）：`push` 喂帧、`poll` 产出当前可
/// 消费的帧；有损路径的排序/抖动缓冲/关键帧重对齐是模块内部事务。
pub trait Interpreter: Send {
    /// 本模块的 pick 规则（装载时确认，与协商结果一致）。
    fn rule(&self) -> PickRule;
    /// 喂入一帧（时间由调用方注入，便于测试与多轨对齐）。
    fn push(&mut self, frame: Frame, now: Instant);
    /// 产出当前可消费的帧（每帧到达后立即 poll 的调用方语义不变）。
    fn poll(&mut self, now: Instant) -> Vec<Frame>;
}

/// 严格即时解读（RealtimePacing）：视频/音频实时目标默认。
///
/// 低延迟、容忍丢帧丢块（关键帧对齐自愈）。内部复用 [`StreamChannel`]：
/// 无损传输（WS/QUIC 全序不丢）直通零延迟；有损/自适应（WebRTC/SRT）经
/// 双轨抖动缓冲按序/按关键帧对齐产出——与既有接收链路逐项等价。
pub struct RealtimePacing {
    rule: PickRule,
    channel: StreamChannel,
}

impl RealtimePacing {
    /// 新建严格即时解读模块（`kind` 由传输可靠性契约决定，见
    /// [`super::manager::channel_kind_for_url`]）。
    pub fn new(kind: ChannelKind) -> Self {
        Self {
            rule: PickRule::Realtime,
            channel: StreamChannel::new(kind),
        }
    }

    /// 两轨抖动缓冲统计（有损路径有效；诊断用）。
    pub const fn jitter_stats(&self) -> (JitterStats, JitterStats) {
        self.channel.stats()
    }
}

impl Interpreter for RealtimePacing {
    fn rule(&self) -> PickRule {
        self.rule
    }

    fn push(&mut self, frame: Frame, now: Instant) {
        self.channel.push(frame, now);
    }

    fn poll(&mut self, now: Instant) -> Vec<Frame> {
        self.channel.poll(now)
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
}

impl StrictOrdered {
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            next_seq: None,
            dropped: 0,
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

    fn push(&mut self, frame: Frame, now: Instant) {
        let _ = now;
        // seq 单调校验（u32 回绕安全）：非连续 = 乱序/重复 → 防御式丢弃。
        // 无损传输（QUIC/WS）seq 恒 0（协议帧头注释：无损路径取 0）——
        // 首帧 0 建立起点后，后续 0 视为重复丢弃？不：无损路径帧不携带
        // 有效 seq，此处仅对「显式携带 seq（有损语义）」做校验；seq=0
        // 全部直通（无损路径逐帧有序，重复不可能）。
        if frame.header.seq != 0 {
            match self.next_seq {
                None => self.next_seq = Some(frame.header.seq.wrapping_add(1)),
                Some(next) => {
                    if frame.header.seq != next {
                        self.dropped += 1;
                        return;
                    }
                    self.next_seq = Some(next.wrapping_add(1));
                }
            }
        }
        self.queue.push_back(frame);
    }

    fn poll(&mut self, _now: Instant) -> Vec<Frame> {
        self.queue.drain(..).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
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

    fn now() -> Instant {
        Instant::now()
    }

    #[test]
    fn realtime_pacing_forwards_in_order() {
        let mut a = RealtimePacing::new(ChannelKind::Lossless);
        assert_eq!(a.rule(), PickRule::Realtime);
        let t = now();
        for i in 0..3 {
            a.push(frame(i), t);
        }
        let out = a.poll(t);
        assert_eq!(out.len(), 3, "无损直通按序产出");
    }

    #[test]
    fn strict_ordered_passthrough_and_seq_guard() {
        let mut a = StrictOrdered::new();
        assert_eq!(a.rule(), PickRule::StrictOrdered);
        let t = now();
        // 无损路径 seq=0：全部直通
        for _ in 0..3 {
            a.push(frame(0), t);
        }
        assert_eq!(a.poll(t).len(), 3);
        // 显式 seq：连续通过
        a.push(frame(1), t);
        a.push(frame(2), t);
        assert_eq!(a.poll(t).len(), 2);
        // 乱序（1 → 3 缺 2）：防御式丢弃
        a.push(frame(1), t);
        a.push(frame(3), t);
        assert_eq!(a.poll(t).len(), 1, "seq=1 通过");
        assert_eq!(a.dropped(), 1, "seq=3 乱序丢弃");
    }

    #[test]
    fn two_profiles_run_independently() {
        // 两路不同解读档同时跑：实时模块（有损抖动）与严格顺序模块互不干扰
        let mut rt = RealtimePacing::new(ChannelKind::Lossy);
        let mut so = StrictOrdered::new();
        let t = now();
        // 实时模块：关键帧开头 + 后续帧（抖动缓冲路径）
        rt.push(frame(0), t);
        rt.push(frame(1), t);
        // 严格顺序模块：独立喂入
        so.push(frame(0), t);
        so.push(frame(0), t);
        assert_eq!(rt.poll(t).len(), 2, "实时模块独立产出");
        assert_eq!(so.poll(t).len(), 2, "严格顺序模块独立产出（seq=0 直通）");
        let _ = Duration::ZERO;
    }
}
