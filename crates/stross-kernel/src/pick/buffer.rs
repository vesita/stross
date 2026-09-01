//! 接收端抖动缓冲（jitter buffer）：吸收网络抖动，按序产出媒体帧
//! （pick 规则层解读模块的内部机制，docs/comm-mode-v2.md §3.0）。
//!
//! 需求 docs/requirements.md §4.4：**流式通道**的接收端组件——
//! 定长环形缓冲、按 `seq` 索引与排序、乱序帧落槽等待、超时未齐跳过并
//! 等待关键帧重对齐（视频轨）、内存有界 = 固定容量。
//!
//! 与中继侧的"新观众先收最近关键帧 + Lagged 重对齐"互补：本组件是
//! **接收端本地**的纯逻辑，时间由调用方注入（可单测）。
//!
//! 职责边界：**只服务有损/自适应路径**（WebRTC/SRT 可能乱序或缺帧）。
//! 无损传输（WS/QUIC 全序不丢）由 [`super::manager::StreamChannel`]
//! 直通处理，不经过本组件（`seq` 语义见协议帧头注释）。

use std::time::{Duration, Instant};

use stross_proto::frame::Frame;

/// 抖动缓冲配置。
#[derive(Debug, Clone, Copy)]
pub struct JitterConfig {
    /// 环形槽数（内存上界 = 槽数 × 单帧大小）。
    pub capacity: usize,
    /// 空洞等待窗口上界：超过该时长未补上的空洞视为丢帧。
    pub max_wait: Duration,
    /// 空洞等待窗口下界（自适应时使用：抖动小 → 贴近此值，低延迟）。
    pub min_wait: Duration,
    /// 自适应等待窗口：按实测到达间隔抖动动态收紧/放宽（音频轨用此收紧
    /// 到 ≤ [`Self::max_wait`]，需求 §4.4「自适应策略」）。
    pub adaptive: bool,
    /// 视频轨丢帧后需等待关键帧重对齐（音频轨为 `false`）。
    pub require_keyframe_resync: bool,
}

impl Default for JitterConfig {
    fn default() -> Self {
        Self {
            capacity: 512,
            max_wait: Duration::from_millis(200),
            min_wait: Duration::from_millis(10),
            adaptive: true,
            require_keyframe_resync: true,
        }
    }
}

/// 抖动缓冲统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JitterStats {
    /// 收到帧数。
    pub received: u64,
    /// 产出帧数。
    pub emitted: u64,
    /// 空洞超时跳过的帧数。
    pub dropped_out_of_window: u64,
    /// 重对齐期间丢弃的非关键帧数。
    pub dropped_resync: u64,
    /// 过期/重复被丢弃的帧数。
    pub dropped_stale: u64,
}

/// 环形槽中的一帧。
struct Slot {
    frame: Frame,
}

/// 接收端抖动缓冲（单轨）。
pub struct JitterBuffer {
    cfg: JitterConfig,
    slots: Vec<Option<Slot>>,
    /// 连续输出游标：下一个应输出的 `seq`。
    next_seq: Option<u32>,
    /// 已到达帧的最高 `seq`（跳洞推进上界：游标不得越过它，否则会
    /// 超前跳过后续已到达的帧——SRT 跨轨共享 seq 时占位空洞密集，
    /// 过度跳洞会把 next_seq 推到所有已到达帧之前，导致帧全部被吞）。
    highest_seq: Option<u32>,
    /// 游标最近一次推进的时间（空洞超时判定基准）。
    last_progress: Option<Instant>,
    /// 重对齐等待：视频轨丢帧后置位，丢弃非关键帧直到关键帧到来。
    awaiting_keyframe: bool,
    /// 到达间隔 EWMA（自适应基准）。
    avg_interval: Option<Duration>,
    /// 抖动估计（到达间隔偏差 EWMA）。
    jitter_est: Duration,
    /// 最近一次 push 时间（间隔计算）。
    last_arrival: Option<Instant>,
    /// 统计。
    pub stats: JitterStats,
}

/// EWMA 更新（α = 1/8 的整数近似；`cur` 为旧值，`sample` 为新样本）。
fn ewma(cur: Duration, sample: Duration) -> Duration {
    let diff = sample.abs_diff(cur);
    if sample >= cur {
        cur + diff / 8
    } else {
        cur - diff / 8
    }
}

/// `seq` 序比较（u32 回绕安全）：`a < b` 当且仅当 `a` 落后于 `b` 不超过半个序号空间。
const fn seq_lt(a: u32, b: u32) -> bool {
    a.wrapping_sub(b) >= (1 << 31)
}

impl JitterBuffer {
    pub fn new(cfg: JitterConfig) -> Self {
        Self {
            slots: (0..cfg.capacity.max(1)).map(|_| None).collect(),
            next_seq: None,
            highest_seq: None,
            last_progress: None,
            awaiting_keyframe: false,
            avg_interval: None,
            jitter_est: Duration::ZERO,
            last_arrival: None,
            stats: JitterStats::default(),
            cfg,
        }
    }

    /// 当前生效的空洞等待窗口：自适应时按实测抖动在 `[min_wait, max_wait]`
    /// 内动态取值（抖动小 → 贴近 `min_wait`，低延迟；抖动大 → 放宽防卡顿）。
    pub(crate) fn effective_wait(&self) -> Duration {
        if !self.cfg.adaptive {
            return self.cfg.max_wait;
        }
        let base = self.jitter_est * 4; // 4× 实测抖动，覆盖多数乱序窗口
        base.clamp(self.cfg.min_wait, self.cfg.max_wait)
    }

    /// 喂入一帧（时间由调用方注入，便于测试与多轨对齐）。
    pub fn push(&mut self, frame: Frame, now: Instant) {
        self.stats.received += 1;
        // 自适应：更新到达间隔与抖动估计（EWMA）
        if let Some(last) = self.last_arrival {
            let interval = now.duration_since(last);
            self.avg_interval = Some(match self.avg_interval {
                Some(avg) => ewma(avg, interval),
                None => interval,
            });
            let dev = match self.avg_interval {
                Some(avg) if interval >= avg => interval - avg,
                Some(avg) => avg - interval,
                None => Duration::ZERO,
            };
            self.jitter_est = ewma(self.jitter_est, dev);
        }
        self.last_arrival = Some(now);
        let seq = frame.header.seq;

        // 关键帧 = 重对齐点：清空旧槽、重置游标
        if frame.header.is_keyframe() {
            if let Some(next) = self.next_seq
                && seq_lt(seq, next)
            {
                self.stats.dropped_stale += 1; // 过期关键帧
                return;
            }
            self.slots.iter_mut().for_each(|s| *s = None);
            self.next_seq = Some(seq);
            self.highest_seq = Some(seq); // 槽已清空，游标上界随之重置
            self.awaiting_keyframe = false;
            self.last_progress = Some(now);
            self.place(seq, Slot { frame });
            return;
        }

        // 等待关键帧重对齐期间：丢弃非关键帧
        if self.awaiting_keyframe {
            self.stats.dropped_resync += 1;
            return;
        }
        match self.next_seq {
            None => {
                // 首帧（无游标）：以该帧为输出起点（有损首帧通常已是关键帧；
                // 兜底允许非关键帧首帧，避免永久无输出）
                self.next_seq = Some(seq);
                self.last_progress = Some(now);
            }
            Some(next) => {
                // 过期帧（游标已越过）：丢弃
                if seq_lt(seq, next) {
                    self.stats.dropped_stale += 1;
                    return;
                }
            }
        }
        self.highest_seq = Some(
            self.highest_seq
                .map_or(seq, |h| if seq_lt(h, seq) { seq } else { h }),
        );
        self.place(seq, Slot { frame });
    }

    /// 放入槽位；槽冲突时保留更新的帧（旧帧作废）。
    fn place(&mut self, seq: u32, slot: Slot) {
        let idx = seq as usize % self.cfg.capacity;
        if let Some(existing) = self.slots[idx].as_ref() {
            if seq_lt(existing.frame.header.seq, seq) {
                self.stats.dropped_stale += 1; // 新帧更新，旧帧作废
            } else {
                self.stats.dropped_stale += 1; // 新帧过期
                return;
            }
        }
        self.slots[idx] = Some(slot);
    }

    /// 产出当前可播放的帧：从游标连续输出；空洞超时则跳过。
    pub fn poll(&mut self, now: Instant) -> Vec<Frame> {
        // 视频轨丢帧后等待关键帧重对齐：期间不产出任何帧
        if self.awaiting_keyframe {
            return Vec::new();
        }
        let Some(next) = self.next_seq else {
            return Vec::new();
        };
        let idx = next as usize % self.cfg.capacity;
        let ready = self.slots[idx]
            .as_ref()
            .is_some_and(|s| s.frame.header.seq == next);
        if !ready {
            let overdue = self
                .last_progress
                .is_some_and(|t| now.duration_since(t) > self.effective_wait());
            if !overdue {
                return Vec::new();
            }
            if let Some(h) = self.highest_seq
                && seq_lt(h, next)
            {
                return Vec::new();
            }
        }
        let mut out = Vec::new();
        // 跳洞上限（= 槽位数）：一次 poll 内最多推进这么多空洞就退出本轮，
        // 防止「空洞持续 + overdue 恒真」时 `next_seq` 无限递增（u32 环绕）
        // 造成的 CPU 死循环（poll 内 `now`/`last_progress` 不变，overdue 不退场）。
        let max_holes = self.cfg.capacity as u32;
        let mut holes_skipped = 0u32;
        while let Some(next) = self.next_seq {
            let idx = next as usize % self.cfg.capacity;
            let ready = self.slots[idx]
                .as_ref()
                .is_some_and(|s| s.frame.header.seq == next);
            if ready {
                let slot = self.slots[idx].take().expect("已检查就绪");
                self.next_seq = Some(next.wrapping_add(1));
                self.last_progress = Some(now);
                self.stats.emitted += 1;
                out.push(slot.frame);
                continue;
            }
            // 空洞：等待窗口内不跳，超时才跳（自适应窗口见 [`Self::effective_wait`]）
            let overdue = self
                .last_progress
                .is_some_and(|t| now.duration_since(t) > self.effective_wait());
            if !overdue {
                break;
            }
            // 游标已越过所有已到达帧：没有更远的目标，退出本轮等新帧
            if let Some(h) = self.highest_seq
                && seq_lt(h, next)
            {
                break;
            }
            if holes_skipped >= max_holes {
                break;
            }
            holes_skipped += 1;
            self.next_seq = Some(next.wrapping_add(1));
            self.stats.dropped_out_of_window += 1;
            if self.cfg.require_keyframe_resync {
                // 视频轨：空洞=丢帧 → 重对齐等待（等关键帧重建，防花屏）。
                // 轨内连续 seq（StreamChannel 已归一化）下占位空洞不存在，
                // 此处只有真丢帧/断流才会触发。
                self.awaiting_keyframe = true;
                break;
            }
            // 音频轨：跳洞后继续推进（受 `highest_seq` 上界约束，游标不
            // 越过已到达帧，故不会超前吞帧；一次 poll 可追平最新帧，且
            // 不会因 `overdue` 恒真而无限递增 `next_seq`）。
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stross_proto::frame::{FLAG_KEYFRAME, TRACK_AUDIO, TRACK_VIDEO};

    fn frame(seq: u32, keyframe: bool) -> Frame {
        Frame::with_seq(
            TRACK_VIDEO,
            stross_proto::frame::CODEC_H264,
            if keyframe { FLAG_KEYFRAME } else { 0 },
            0,
            seq,
            vec![0x41, 0x00, 0x01],
        )
    }

    fn audio_frame(seq: u32) -> Frame {
        Frame::with_seq(
            TRACK_AUDIO,
            stross_proto::frame::CODEC_AAC,
            0,
            0,
            seq,
            vec![0xFF, 0xF1, 0x50],
        )
    }

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn ordered_stream_emits_in_order() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let now = t0();
        for i in 0..5 {
            jb.push(frame(i, i == 0), now);
        }
        let out = jb.poll(now);
        let seqs: Vec<u32> = out.iter().map(|f| f.header.seq).collect();
        assert_eq!(seqs, vec![0, 1, 2, 3, 4]);
        assert_eq!(jb.stats.emitted, 5);
    }

    #[test]
    fn out_of_order_frames_waited_then_emitted() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let now = t0();
        jb.push(frame(0, true), now);
        jb.push(frame(2, false), now);
        jb.push(frame(3, false), now);
        // seq1 缺失：只能连续输出到 0
        assert_eq!(jb.poll(now).len(), 1);
        // seq1 补上 → 1,2,3 连续输出
        jb.push(frame(1, false), now);
        let out = jb.poll(now);
        let seqs: Vec<u32> = out.iter().map(|f| f.header.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
    }

    #[test]
    fn hole_timeout_skips_and_video_resyncs_on_keyframe() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let now = t0();
        jb.push(frame(0, true), now);
        jb.push(frame(2, false), now);
        assert_eq!(jb.poll(now).len(), 1); // 只出 0，seq1 空洞未超时
        // 超时后 poll：跳 seq1，视频轨进入重对齐等待
        let later = now + JitterConfig::default().max_wait + Duration::from_millis(10);
        assert!(jb.poll(later).is_empty(), "重对齐期间不产出非关键帧");
        assert_eq!(jb.stats.dropped_out_of_window, 1);
        // 非关键帧（seq2）在重对齐期间被丢弃
        jb.push(frame(2, false), later);
        assert_eq!(jb.stats.dropped_resync, 1);
        // 关键帧到来 → 重对齐，重新从该关键帧输出
        jb.push(frame(10, true), later);
        let out = jb.poll(later);
        let seqs: Vec<u32> = out.iter().map(|f| f.header.seq).collect();
        assert_eq!(seqs, vec![10]);
        assert!(!jb.awaiting_keyframe);
    }

    #[test]
    fn audio_track_skips_hole_without_resync() {
        let mut jb = JitterBuffer::new(JitterConfig {
            require_keyframe_resync: false,
            ..Default::default()
        });
        let now = t0();
        jb.push(audio_frame(0), now);
        jb.push(audio_frame(2), now);
        assert_eq!(jb.poll(now).len(), 1);
        let later = now + JitterConfig::default().max_wait + Duration::from_millis(10);
        let out = jb.poll(later);
        let seqs: Vec<u32> = out.iter().map(|f| f.header.seq).collect();
        assert_eq!(seqs, vec![2], "音频轨跳洞后继续输出");
        assert!(!jb.awaiting_keyframe);
    }

    #[test]
    fn stale_duplicate_frames_dropped() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let now = t0();
        jb.push(frame(0, true), now);
        jb.push(frame(0, false), now); // 重复 seq0：槽冲突保留新帧
        assert_eq!(jb.stats.dropped_stale, 1);
        let out = jb.poll(now);
        assert_eq!(out.len(), 1);
        // 已输出 seq0 后，seq0 再来 → 过期
        jb.push(frame(0, false), now);
        assert_eq!(jb.stats.dropped_stale, 2);
        assert!(jb.poll(now).is_empty());
    }

    #[test]
    fn memory_bounded_regardless_of_push_count() {
        let mut jb = JitterBuffer::new(JitterConfig {
            capacity: 8,
            ..Default::default()
        });
        let now = t0();
        // 推入远超容量的连续帧（含关键帧开头）
        jb.push(frame(0, true), now);
        for i in 1..10_000 {
            jb.push(frame(i, false), now);
        }
        assert_eq!(jb.slots.len(), 8, "环形槽数恒定");
        assert!(jb.slots.iter().flatten().count() <= 8, "槽内帧数 ≤ 容量");
    }

    #[test]
    fn adaptive_wait_tracks_low_jitter() {
        // 稳定节拍（30fps ≈ 33ms）→ 抖动估计小 → 等待窗口贴近 min_wait
        let mut jb = JitterBuffer::new(JitterConfig {
            adaptive: true,
            max_wait: Duration::from_millis(200),
            min_wait: Duration::from_millis(10),
            ..Default::default()
        });
        let mut t = t0();
        for i in 0..20 {
            jb.push(frame(i, i == 0), t);
            t += Duration::from_millis(33);
        }
        assert!(
            jb.effective_wait() <= Duration::from_millis(60),
            "低抖动下等待窗口应收紧，实际 {:?}",
            jb.effective_wait()
        );
    }

    #[test]
    fn adaptive_wait_expands_under_high_jitter() {
        // 抖动注入（间隔 5ms / 61ms 交替，均值 33ms）→ 抖动估计大 → 窗口放宽
        let mut jb = JitterBuffer::new(JitterConfig {
            adaptive: true,
            max_wait: Duration::from_millis(200),
            min_wait: Duration::from_millis(10),
            ..Default::default()
        });
        let mut t = t0();
        for i in 0..20 {
            jb.push(frame(i, i == 0), t);
            t += if i % 2 == 0 {
                Duration::from_millis(5)
            } else {
                Duration::from_millis(61)
            };
        }
        assert!(
            jb.effective_wait() >= Duration::from_millis(80),
            "高抖动下等待窗口应放宽，实际 {:?}",
            jb.effective_wait()
        );
    }

    #[test]
    fn seq_lt_respects_u32_wraparound() {
        // 直接验证序比较在回绕边界两侧的语义（半个序号空间内判断先后）
        assert!(seq_lt(5, 6), "普通先后：5 落后于 6");
        assert!(!seq_lt(6, 5), "普通先后：6 不落后于 5");
        assert!(seq_lt(u32::MAX, 0), "回绕边界：u32::MAX 落后于 0");
        assert!(!seq_lt(0, u32::MAX), "回绕边界：0 不落后于 u32::MAX");
        assert!(seq_lt(1, 1 << 30), "半个空间内正常先后");
        assert!(!seq_lt(1 << 30, 1), "半个空间内正常先后");
    }

    #[test]
    fn emits_sequentially_across_wraparound() {
        // 流在 u32::MAX 附近回绕：游标 wrapping_add 平滑越过边界，逐帧连续产出
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let now = t0();
        let start = u32::MAX - 3; // 覆盖 u32::MAX → 0 → 1 的回绕
        jb.push(frame(start, true), now);
        for i in 1..=6 {
            jb.push(frame(start.wrapping_add(i), false), now);
        }
        let out = jb.poll(now);
        let seqs: Vec<u32> = out.iter().map(|f| f.header.seq).collect();
        assert_eq!(
            seqs,
            vec![u32::MAX - 3, u32::MAX - 2, u32::MAX - 1, u32::MAX, 0, 1, 2],
            "跨回绕应连续产出"
        );
        assert_eq!(jb.stats.emitted, 7);
    }

    #[test]
    fn late_frame_before_wrap_dropped_as_stale() {
        // 游标已越过回绕点后，迟到的回绕前帧按过期丢弃（不得乱序插入）
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let now = t0();
        jb.push(frame(u32::MAX - 1, true), now);
        jb.push(frame(u32::MAX, false), now);
        jb.push(frame(0, false), now);
        jb.push(frame(1, false), now);
        assert_eq!(jb.poll(now).len(), 4, "回绕后连续输出，游标现为 2");
        // 迟到的回绕前帧（应排在游标之前）→ 过期
        jb.push(frame(u32::MAX - 2, false), now);
        assert_eq!(jb.stats.dropped_stale, 1);
        assert!(jb.poll(now).is_empty());
    }

    #[test]
    fn stale_keyframe_before_wrap_does_not_resync() {
        // 回绕边界之后的旧关键帧不得重置游标（防倒带回绕重对齐）
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let now = t0();
        jb.push(frame(u32::MAX - 1, true), now);
        jb.push(frame(u32::MAX, false), now);
        jb.push(frame(0, false), now);
        jb.push(frame(1, false), now);
        assert_eq!(jb.poll(now).len(), 4, "游标推进到 2");
        jb.push(frame(u32::MAX - 2, true), now); // 迟到的回绕前关键帧
        assert_eq!(jb.stats.dropped_stale, 1, "旧关键帧应丢弃而非重对齐");
        assert_eq!(
            jb.stats.emitted, 4,
            "poll 已输出 4 帧，旧关键帧不得重置游标"
        );
        assert!(jb.poll(now).is_empty());
    }

    #[test]
    fn hole_across_wraparound_times_out_and_resyncs() {
        // 空洞恰好跨越回绕边界（u32::MAX 缺失）：视频轨超时跳洞后进入重对齐等待
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let now = t0();
        jb.push(frame(u32::MAX - 1, true), now);
        jb.push(frame(0, false), now); // u32::MAX 缺失（空洞）
        assert_eq!(jb.poll(now).len(), 1, "空洞未超时只出 u32::MAX-1");
        let later = now + JitterConfig::default().max_wait + Duration::from_millis(10);
        assert!(jb.poll(later).is_empty(), "跳洞后进入重对齐等待");
        assert_eq!(jb.stats.dropped_out_of_window, 1);
        // 关键帧（回绕后）到来 → 重对齐恢复
        jb.push(frame(5, true), later);
        let out = jb.poll(later);
        let seqs: Vec<u32> = out.iter().map(|f| f.header.seq).collect();
        assert_eq!(seqs, vec![5], "重对齐后从新关键帧恢复输出");
    }

    #[test]
    fn adaptive_window_gap_waited_then_emitted() {
        // 自适应窗口内的空洞应等待（不跳过），窗口外超时才跳
        let mut jb = JitterBuffer::new(JitterConfig {
            adaptive: true,
            max_wait: Duration::from_millis(200),
            min_wait: Duration::from_millis(10),
            ..Default::default()
        });
        let now = t0();
        // 首帧 + 后续稳定节拍建立抖动估计（低抖动 → 窗口 ~10-60ms）
        jb.push(frame(0, true), now);
        let mut t = now + Duration::from_millis(33);
        for i in 1..6 {
            jb.push(frame(i, false), t);
            t += Duration::from_millis(33);
        }
        let wait = jb.effective_wait();
        // 空洞（seq6 缺失）在窗口内不跳
        assert!(jb.poll(now + wait / 2).len() <= 6, "窗口内不跳洞");
        // 补上 seq6 → 连续输出
        jb.push(frame(6, false), t);
        let out = jb.poll(t);
        let seqs: Vec<u32> = out.iter().map(|f| f.header.seq).collect();
        assert_eq!(seqs, vec![6], "补上后应输出 seq6");
    }
}
