//! 流式通道（RealtimePacing 内部机制）与解读模块注册表
//! （需求 docs/requirements.md §4.4 / F4）。
//!
//! 每条会话（session_id）一个**解读模块**（pick 规则层，通信模式 v2，
//! docs/comm-mode-v2.md §3.0）：
//!
//! * [`RealtimePacing`](super::interpret::RealtimePacing)（默认）：媒体帧
//!   严格即时解读——按会话协商出的传输可靠性分流（无损直通 / 有损经
//!   [抖动缓冲](super::buffer::JitterBuffer) 按序/按关键帧对齐产出）；
//! * [`StrictOrdered`](super::interpret::StrictOrdered)：文件分块 / 剪贴板 /
//!   输入——严格顺序、逐字节不丢（Lossless + seq 单调校验）。
//!
//! [`InterpretRegistry`] 是会话级容器：以 `session_id` 装载/索引各解读模块，
//! 生命周期与会话一致（`adapter()` 于会话建立、`remove()` 于会话拆除）。

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use stross_proto::frame::{Frame, TRACK_VIDEO};
use stross_proto::message::PickRule;

use super::buffer::{JitterBuffer, JitterConfig};
use super::interpret::{Interpreter, RealtimePacing, StrictOrdered};

/// 流式通道的数据路径（由会话协商出的传输可靠性决定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    /// 无损（WS/QUIC）：全序不丢，直通不过抖动缓冲。
    Lossless,
    /// 有损/自适应（WebRTC/SRT）：可能乱序或缺帧，进抖动缓冲。
    Lossy,
}

/// 按 relay URL scheme 的传输可靠性契约分流（SRT = Adaptive → 有损路径进
/// 抖动缓冲；WS/QUIC = Lossless → 直通零延迟）。推流端 `RelayClient` /
/// 观看端 `connect_watch` 的同一 scheme 判断见 [`crate::transport::transport_for_url`]。
pub fn channel_kind_for_url(relay_url: &str) -> ChannelKind {
    match crate::transport::transport_for_url(relay_url).profile() {
        stross_proto::message::ReliabilityProfile::Adaptive => ChannelKind::Lossy,
        _ => ChannelKind::Lossless,
    }
}

/// 流式通道：按 `ChannelKind` 分流到直通队列或双轨抖动缓冲。
pub struct StreamChannel {
    kind: ChannelKind,
    /// 无损直通队列（有序到达，按序产出）。
    lossless_queue: VecDeque<Frame>,
    video: JitterBuffer,
    audio: JitterBuffer,
    /// 轨内序号：把传输层**跨轨共享**的全局 seq（SRT：视频帧间夹音频 seq）
    /// 归一化为每轨独立的连续序号再喂抖动缓冲——抖动缓冲假设「空洞=丢帧」
    /// （轨内连续 seq 语义，如 WebRTC）；直接喂全局 seq 会把音频占位空洞
    /// 误判为丢帧（等待窗口耗尽 → 重对齐吞帧 / 跳洞游标落后被关键帧清空）。
    video_seq: u32,
    audio_seq: u32,
}

impl StreamChannel {
    /// 新建通道（kind 由会话协商结果决定）。
    pub fn new(kind: ChannelKind) -> Self {
        Self {
            kind,
            lossless_queue: VecDeque::new(),
            video: JitterBuffer::new(JitterConfig {
                require_keyframe_resync: true,
                // 轨内连续 seq：空洞即真丢帧，保持「空洞即重对齐」防花屏
                ..JitterConfig::default()
            }),
            // 音频轨收紧：低延迟预算（端到端 ≤200ms 的一部分）≤100ms，
            // 自适应在抖动小时贴近 min_wait（需求 §4.4 自适应策略）
            audio: JitterBuffer::new(JitterConfig {
                require_keyframe_resync: false,
                max_wait: Duration::from_millis(100),
                min_wait: Duration::from_millis(10),
                adaptive: true,
                ..JitterConfig::default()
            }),
            video_seq: 0,
            audio_seq: 0,
        }
    }

    /// 喂入一帧。
    pub fn push(&mut self, frame: Frame, now: Instant) {
        match self.kind {
            ChannelKind::Lossless => self.lossless_queue.push_back(frame),
            ChannelKind::Lossy => {
                let mut frame = frame;
                // 轨内序号归一化（见 [`Self::video_seq`] 注释）；输出帧沿用
                // 轨内序号，消费方（解码/延迟统计）依赖 pts 而非全局 seq。
                let seq = if frame.header.track == TRACK_VIDEO {
                    let s = self.video_seq;
                    self.video_seq = self.video_seq.wrapping_add(1);
                    s
                } else {
                    let s = self.audio_seq;
                    self.audio_seq = self.audio_seq.wrapping_add(1);
                    s
                };
                frame.header.seq = seq;
                match frame.header.track {
                    TRACK_VIDEO => self.video.push(frame, now),
                    _ => self.audio.push(frame, now),
                }
            }
        }
    }

    /// 产出当前可播放的帧（视频 + 音频合并；接收端按 `track` 分流消费）。
    pub fn poll(&mut self, now: Instant) -> Vec<Frame> {
        match self.kind {
            ChannelKind::Lossless => self.lossless_queue.drain(..).collect(),
            ChannelKind::Lossy => {
                let mut out = self.video.poll(now);
                out.extend(self.audio.poll(now));
                out
            }
        }
    }

    /// 两轨抖动缓冲统计（有损路径有效；诊断用）。
    pub const fn stats(&self) -> (super::buffer::JitterStats, super::buffer::JitterStats) {
        (self.video.stats, self.audio.stats)
    }
}

/// 解读模块注册表：按会话装载 pick 规则解读模块。
#[derive(Default)]
pub struct InterpretRegistry {
    interpreters: HashMap<String, Box<dyn Interpreter>>,
}

impl InterpretRegistry {
    /// 会话建立时创建（或复用）其解读模块。
    ///
    /// `rule`：协商定稿的 pick 规则（订阅握手携带；通信模式 v2）——
    /// [`PickRule::StrictOrdered`]（文件/剪贴板）装载严格顺序模块，
    /// 其余（Realtime/None）装载严格即时模块。`kind` 由传输可靠性契约
    /// 决定（实时模块内部的无损直通 / 有损抖动分流）。
    pub fn adapter(
        &mut self,
        session_id: &str,
        rule: PickRule,
        kind: ChannelKind,
    ) -> &mut dyn Interpreter {
        let key = (rule, kind);
        let slot = self
            .interpreters
            .entry(session_id.to_string())
            .or_insert_with(|| match key {
                (PickRule::StrictOrdered, _) => {
                    Box::new(StrictOrdered::new()) as Box<dyn Interpreter>
                }
                (_, kind) => Box::new(RealtimePacing::new(kind)) as Box<dyn Interpreter>,
            });
        slot.as_mut()
    }

    /// 会话拆除时移除（释放缓冲）。
    pub fn remove(&mut self, session_id: &str) {
        self.interpreters.remove(session_id);
    }

    /// 当前活跃会话数。
    pub fn len(&self) -> usize {
        self.interpreters.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.interpreters.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stross_proto::frame::{FLAG_KEYFRAME, TRACK_AUDIO};

    fn frame(track: u8, seq: u32, keyframe: bool) -> Frame {
        Frame::with_seq(
            track,
            if track == TRACK_VIDEO {
                stross_proto::frame::CODEC_H264
            } else {
                stross_proto::frame::CODEC_AAC
            },
            if keyframe { FLAG_KEYFRAME } else { 0 },
            0,
            seq,
            vec![0x41, 0x00, 0x01],
        )
    }

    #[test]
    fn lossless_channel_passthrough_in_order() {
        let mut ch = StreamChannel::new(ChannelKind::Lossless);
        let now = Instant::now();
        for _ in 0..3 {
            ch.push(frame(TRACK_VIDEO, 0, false), now); // 无损路径 seq 恒 0，按到达顺序
        }
        let out = ch.poll(now);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn lossy_channel_routes_by_track() {
        let mut ch = StreamChannel::new(ChannelKind::Lossy);
        let now = Instant::now();
        ch.push(frame(TRACK_VIDEO, 0, true), now);
        ch.push(frame(TRACK_AUDIO, 0, false), now);
        let out = ch.poll(now);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn manager_loads_adapter_by_rule() {
        let mut m = InterpretRegistry::default();
        assert!(m.is_empty());
        // 严格即时规则（默认/媒体）：RealtimePacing 模块
        m.adapter("sess-1", PickRule::Realtime, ChannelKind::Lossy)
            .push(frame(TRACK_AUDIO, 0, false), Instant::now());
        // 严格顺序规则（文件/剪贴板）：StrictOrdered 模块
        m.adapter("sess-2", PickRule::StrictOrdered, ChannelKind::Lossless)
            .push(frame(TRACK_AUDIO, 0, false), Instant::now());
        assert_eq!(m.len(), 2);
        assert_eq!(
            m.adapter("sess-1", PickRule::Realtime, ChannelKind::Lossy)
                .rule(),
            PickRule::Realtime
        );
        assert_eq!(
            m.adapter("sess-2", PickRule::StrictOrdered, ChannelKind::Lossless)
                .rule(),
            PickRule::StrictOrdered
        );
        m.remove("sess-1");
        assert_eq!(m.len(), 1);
        // sess-2 仍存活（按 id 装载/拆除互不级联）
        assert!(
            m.adapter("sess-2", PickRule::StrictOrdered, ChannelKind::Lossless)
                .rule()
                == PickRule::StrictOrdered
        );
    }

    #[test]
    fn manager_lifecycle() {
        let mut m = InterpretRegistry::default();
        assert!(m.is_empty());
        m.adapter("sess-1", PickRule::Realtime, ChannelKind::Lossy)
            .push(frame(TRACK_AUDIO, 0, false), Instant::now());
        m.adapter("sess-2", PickRule::Realtime, ChannelKind::Lossless);
        assert_eq!(m.len(), 2);
        m.remove("sess-1");
        assert_eq!(m.len(), 1);
        assert!(
            m.adapter("sess-2", PickRule::Realtime, ChannelKind::Lossless)
                .rule()
                == PickRule::Realtime
        );
    }
}
