//! PTS 驱动播放调度层（纯逻辑，线程外壳在 [`super::ffmpeg`]）。
//!
//! 播放侧此前无调度层——帧进即解即出，显示节奏随网络抖动/解码耗时波动，
//! 端到端延迟不可预期（iteration-plan 遗留项）。本模块把解码帧按**源节奏**
//! （pts 相对间距）调度输出：
//!
//! * **锚定**：首帧到达时刻 + 首帧 pts → 播放时钟 `play(pts) = anchor + (pts − pts0)`；
//! * **等待**：play 时刻在未来的帧入队，到点再发（吸收抖动，显示平滑）；
//! * **过水位丢帧**：队尾（最新帧）play 时刻晚于 `now + target_delay` →
//!   丢队尾，把显示延迟钳制在目标水位（发送端过快 / 时钟漂移时追平实时；
//!   正常流零丢帧零加时）。RGBA 显示帧无解码依赖，丢帧不损坏后续解码；
//! * **大 PTS 跳变**（> `jump_reset`）：重置锚点 + 清空缓冲重对齐
//!   （流切换 / 重连 / 失步重建）；
//! * **迟到帧**（play 时刻已过）：立即发出（欠水位不补帧——上一帧静画由
//!   消费端自然保持；音频插静音留给 AV Sync 阶段，见 roadmap P3）。
//!
//! 纯逻辑、时间注入（`now: Instant` 由调用方传入），可单测；仅实时显示
//! 路径启用，录制 / headless 直通不经过本模块。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::RenderedFrame;

/// 调度统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SchedulerStats {
    /// 喂入帧数。
    pub received: u64,
    /// 按时发出帧数（含迟到立即发出）。
    pub emitted: u64,
    /// 在缓冲中等待过（play 时刻在未来）后发出的帧数——调度生效的直观指标。
    pub held: u64,
    /// 过水位丢帧数（超前追平）。
    pub dropped_watermark: u64,
    /// 锚点重置时清空的缓冲帧数。
    pub dropped_reset: u64,
    /// pts 回退（< 锚点 pts）丢弃帧数。
    pub dropped_stale: u64,
    /// 锚点重置次数。
    pub reanchors: u64,
}

/// 队列中的一帧（play 时刻已算好）。
struct Queued {
    frame: RenderedFrame,
    play_at: Instant,
}

/// PTS 驱动播放调度器（单会话、单轨）。
pub struct PlaybackScheduler {
    target_delay: Duration,
    jump_reset: Duration,
    /// 锚点：首帧 (pts, 到达时刻)。
    anchor_pts: Option<u32>,
    anchor_at: Option<Instant>,
    /// 按 play_at 升序的待发队列（内存上界 = target_delay 内帧数）。
    queue: VecDeque<Queued>,
    /// 统计。
    pub stats: SchedulerStats,
}

impl PlaybackScheduler {
    /// 队列最大帧数上限（防呆：防止异常时间戳导致队列无限膨胀）。
    const MAX_QUEUE_LEN: usize = 120;

    pub fn new(target_delay: Duration, jump_reset: Duration) -> Self {
        Self {
            target_delay,
            jump_reset,
            anchor_pts: None,
            anchor_at: None,
            queue: VecDeque::new(),
            stats: SchedulerStats::default(),
        }
    }

    /// 队首 play 时刻（线程用其作为下一次阻塞等待的目标；无帧 = `None`）。
    pub fn next_play_at(&self) -> Option<Instant> {
        self.queue.front().map(|q| q.play_at)
    }

    /// 喂入一帧解码画面（pts 为协议帧头毫秒时间戳）。
    ///
    /// 帧被入队等待或立即作为"迟到帧"待发；调用方随后调
    /// [`Self::emit_due`] + [`Self::drop_over_watermark`] 推进。
    pub fn push(&mut self, frame: RenderedFrame, now: Instant) {
        self.stats.received += 1;
        let pts = frame.pts_ms;
        match (self.anchor_pts, self.anchor_at) {
            (None, _) | (_, None) => {
                // 首帧：锚定并立即播放（低延迟起步，不额外等一个 target_delay）
                self.anchor_pts = Some(pts);
                self.anchor_at = Some(now);
                self.queue.push_back(Queued {
                    frame,
                    play_at: now,
                });
            }
            (Some(p0), Some(t0)) => {
                if pts < p0 {
                    let back_delta = Duration::from_millis(u64::from(p0 - pts));
                    if back_delta > self.jump_reset {
                        // 向后大跳变（流重置 / 循环播放 / 推流端重启）：重置缓冲并重新锚定
                        self.stats.reanchors += 1;
                        self.stats.dropped_reset += self.queue.len() as u64;
                        self.queue.clear();
                        self.anchor_pts = Some(pts);
                        self.anchor_at = Some(now);
                        self.queue.push_back(Queued {
                            frame,
                            play_at: now,
                        });
                        return;
                    }
                    // 较小的向后乱序/回退帧：过期丢弃
                    self.stats.dropped_stale += 1;
                    return;
                }
                let delta = Duration::from_millis(u64::from(pts - p0));
                if delta > self.jump_reset {
                    // 向前大跳变：重置缓冲重锚定
                    self.stats.reanchors += 1;
                    self.stats.dropped_reset += self.queue.len() as u64;
                    self.queue.clear();
                    self.anchor_pts = Some(pts);
                    self.anchor_at = Some(now);
                    self.queue.push_back(Queued {
                        frame,
                        play_at: now,
                    });
                    return;
                }
                // 正常：按源节奏调度（迟到帧 play_at ≤ now，emit_due 会立即发）
                let play_at = t0 + delta;
                if play_at > now {
                    self.stats.held += 1;
                }
                self.queue.push_back(Queued { frame, play_at });
            }
        }
        // 防呆：防止队列超过上限帧数
        while self.queue.len() > Self::MAX_QUEUE_LEN {
            self.queue.pop_front();
            self.stats.dropped_watermark += 1;
        }
    }

    /// 发出所有已到 play 时刻的帧（按序；调用方负责 `try_send` 与丢弃计数）。
    pub fn emit_due(&mut self, now: Instant) -> Vec<RenderedFrame> {
        let mut out = Vec::new();
        self.emit_due_into(&mut out, now);
        out
    }

    /// 将所有已到期帧移入调用方提供的缓冲区，避免重复分配。
    pub fn emit_due_into(&mut self, out: &mut Vec<RenderedFrame>, now: Instant) {
        while let Some(head) = self.queue.front() {
            if head.play_at > now {
                break;
            }
            let q = self.queue.pop_front().expect("已检查队首");
            self.stats.emitted += 1;
            out.push(q.frame);
        }
    }

    /// 遍历所有已到期帧并传递给闭包处理，零额外集合分配。
    pub fn emit_due_with<F, E>(&mut self, now: Instant, mut f: F) -> Result<(), E>
    where
        F: FnMut(RenderedFrame) -> Result<(), E>,
    {
        while let Some(head) = self.queue.front() {
            if head.play_at > now {
                break;
            }
            let q = self.queue.pop_front().expect("已检查队首");
            self.stats.emitted += 1;
            f(q.frame)?;
        }
        Ok(())
    }

    /// 过水位丢帧（播放延迟控制器）：**队尾**（最新等待帧）的 play 时刻
    /// 晚于 `now + target_delay` → 丢队尾，把显示延迟钳制在目标水位内。
    ///
    /// 语义依据：正常流（到达节拍 ≈ pts 节拍）队尾延迟 ≈ 0，零丢帧零加时；
    /// 发送端过快/时钟漂移时队尾延迟线性增长，丢**最新**帧让显示稳定落后
    /// `target_delay` 追平实时（标准播放器固定延迟缓冲行为）；一次性超前
    /// 突发（队首已近 now）不受影响，帧按源时间轴正常播出。
    pub fn drop_over_watermark(&mut self, now: Instant) -> u32 {
        let horizon = now + self.target_delay;
        let mut dropped = 0u32;
        while let Some(tail) = self.queue.back() {
            if tail.play_at <= horizon {
                break;
            }
            self.queue.pop_back();
            self.stats.dropped_watermark += 1;
            dropped += 1;
        }
        dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(pts_ms: u32) -> RenderedFrame {
        RenderedFrame {
            pts_ms,
            width: 2,
            height: 2,
            rgba: vec![0u8; 16],
        }
    }

    const T: Duration = Duration::from_millis(150); // 默认 target_delay
    const JUMP: Duration = Duration::from_millis(500);

    /// 构造调度器并喂入 (pts, 相对 t0 到达时刻) 序列。
    fn feed(sched: &mut PlaybackScheduler, frames: &[(u32, u64)], t0: Instant) {
        for (pts, at) in frames {
            sched.push(frame(*pts), t0 + Duration::from_millis(*at));
        }
    }

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn first_frame_plays_immediately_and_anchor_fixes_clock() {
        let mut s = PlaybackScheduler::new(T, JUMP);
        let now = t0();
        // 首帧 pts=1000 在 t0 到达 → 立即发出，锚点 (1000, t0)
        s.push(frame(1000), now);
        assert_eq!(s.emit_due(now).len(), 1);
        assert_eq!(s.stats.emitted, 1);
        // 第二帧 pts=1033（+33ms）同一时刻到达（网络突发）→ play_at = t0+33ms
        s.push(frame(1033), now);
        assert!(s.emit_due(now).is_empty(), "未到 play 时刻不发出");
        assert_eq!(s.stats.emitted, 1);
        // 33ms 后到点发出
        let later = now + Duration::from_millis(33);
        let out = s.emit_due(later);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pts_ms, 1033);
    }

    #[test]
    fn frames_on_cadence_pass_through_without_hold() {
        // 帧按 33ms 节拍逐帧到达（与 play 时刻一致）→ 无等待、零丢弃
        let mut s = PlaybackScheduler::new(T, JUMP);
        let now = t0();
        feed(&mut s, &[(0, 0), (33, 33), (66, 66), (99, 99)], now);
        for i in 0..4 {
            let at = now + Duration::from_millis(33 * i);
            let out = s.emit_due(at);
            assert_eq!(out.len(), 1, "第 {i} 帧应到点即发");
            assert_eq!(out[0].pts_ms, 33 * i as u32);
        }
        assert_eq!(s.stats.dropped_watermark, 0);
        assert_eq!(s.stats.dropped_stale, 0);
        assert_eq!(s.stats.reanchors, 0);
        assert_eq!(s.stats.held, 0, "按节拍到达无需等待");
    }

    #[test]
    fn burst_ahead_is_held_then_drained_on_schedule() {
        // 突发：0ms 到达 4 帧（pts 间距 33ms）→ 全部入队，按锚点时钟逐个发出
        let mut s = PlaybackScheduler::new(T, JUMP);
        let now = t0();
        feed(&mut s, &[(0, 0), (33, 0), (66, 0), (99, 0)], now);
        // 首帧立即发；其余 held
        assert_eq!(s.emit_due(now).len(), 1);
        assert_eq!(s.stats.emitted, 1);
        for i in 1..4 {
            let at = now + Duration::from_millis(33 * i);
            let out = s.emit_due(at);
            assert_eq!(out.len(), 1, "第 {i} 帧按 33ms 节拍发出");
            assert_eq!(out[0].pts_ms, 33 * i as u32);
        }
        assert_eq!(s.stats.emitted, 4);
        assert_eq!(s.stats.dropped_watermark, 0, "突发未超水位不丢帧");
        assert_eq!(s.stats.held, 3, "突发中 3 帧等待过");
    }

    #[test]
    fn sustained_lead_drops_to_watermark() {
        // 发送端持续超前（帧瞬时到达但 pts 覆盖 264ms）→ 队尾延迟超水位，
        // 丢**最新**帧把显示延迟钳制在 target_delay 内
        let mut s = PlaybackScheduler::new(T, JUMP);
        let now = t0();
        feed(&mut s, &[(0, 0), (33, 0), (66, 0), (99, 0), (132, 0)], now);
        assert_eq!(s.emit_due(now).len(), 1);
        // 队尾 play_at = +132ms ≤ 150ms 水位 → 不丢
        assert_eq!(s.drop_over_watermark(now), 0);
        // 继续以 0 到达时刻瞬时喂入 → 队尾延迟持续超限 → 丢队尾追平
        for (pts, at) in [(165u32, 0u32), (198, 0), (231, 0), (264, 0)] {
            s.push(frame(pts), now + Duration::from_millis(u64::from(at)));
        }
        let dropped = s.drop_over_watermark(now);
        assert!(dropped > 0, "超前应丢帧");
        // 丢帧后队尾 play 时刻应回到水位内（显示延迟 ≤ target_delay）
        if let Some(tail) = s.queue.back() {
            assert!(tail.play_at <= now + T, "丢帧后队尾应回到水位内");
        }
        assert_eq!(s.stats.dropped_watermark as u32, dropped);
        // 缓冲内容仍有 ≥2 帧且按序等待（不把旧内容全丢光）
        assert!(s.queue.len() >= 2, "应保留水位内的帧: {}", s.queue.len());
    }

    #[test]
    fn big_pts_jump_resets_buffer_and_reanchors() {
        let mut s = PlaybackScheduler::new(T, JUMP);
        let now = t0();
        feed(&mut s, &[(1000, 0), (1033, 5)], now);
        assert_eq!(s.emit_due(now + Duration::from_millis(40)).len(), 2);
        // 大跳变（+1000ms > 500ms）：清缓冲重锚定
        s.push(frame(2066), now + Duration::from_millis(50));
        assert_eq!(s.stats.reanchors, 1);
        // 缓冲被清空：队首即新帧（立即发）
        assert_eq!(s.emit_due(now + Duration::from_millis(50)).len(), 1);
        assert_eq!(s.stats.dropped_reset, 0, "跳变前无滞留缓冲");
        assert_eq!(s.stats.emitted, 3);
    }

    #[test]
    fn big_pts_jump_discards_pending_buffer() {
        let mut s = PlaybackScheduler::new(T, JUMP);
        let now = t0();
        // 突发 3 帧：首帧已发，2 帧在队
        feed(&mut s, &[(0, 0), (33, 0), (66, 0)], now);
        assert_eq!(s.emit_due(now).len(), 1);
        // 大跳变：队中 2 帧被清空丢弃
        s.push(frame(1000), now + Duration::from_millis(10));
        assert_eq!(s.stats.reanchors, 1);
        assert_eq!(s.stats.dropped_reset, 2);
        assert_eq!(s.emit_due(now + Duration::from_millis(10)).len(), 1);
    }

    #[test]
    fn stale_pts_before_anchor_dropped() {
        let mut s = PlaybackScheduler::new(T, JUMP);
        let now = t0();
        s.push(frame(100), now);
        assert_eq!(s.emit_due(now).len(), 1);
        // 锚点后 pts 回退（中继补发旧关键帧等）→ 丢弃
        s.push(frame(50), now + Duration::from_millis(1));
        assert_eq!(s.stats.dropped_stale, 1);
        assert!(s.emit_due(now + Duration::from_millis(1)).is_empty());
    }

    #[test]
    fn late_frame_emits_immediately() {
        // 帧到达时 play 时刻已过（网络卡顿后追帧）→ 立即发出，不补等
        let mut s = PlaybackScheduler::new(T, JUMP);
        let now = t0();
        s.push(frame(0), now);
        assert_eq!(s.emit_due(now).len(), 1);
        // pts+33 的帧在 100ms 后才到（已迟到 67ms）
        s.push(frame(33), now + Duration::from_millis(100));
        let out = s.emit_due(now + Duration::from_millis(100));
        assert_eq!(out.len(), 1, "迟到帧立即发出");
        assert_eq!(out[0].pts_ms, 33);
        assert_eq!(s.stats.emitted, 2);
    }

    #[test]
    fn empty_scheduler_has_no_play_deadline() {
        let mut s = PlaybackScheduler::new(T, JUMP);
        assert_eq!(s.next_play_at(), None);
        assert!(s.emit_due(t0()).is_empty());
        assert_eq!(s.drop_over_watermark(t0()), 0);
    }

    #[test]
    fn memory_bounded_by_watermark() {
        // 无论喂入多少超前帧，缓冲跨度都被水位限制（丢帧追平）
        let mut s = PlaybackScheduler::new(T, JUMP);
        let t0 = t0();
        let mut pts = 0u32;
        for i in 0..500u64 {
            // 发送端 pts 以 33ms/帧推进，但真实到达仅 5ms/帧 → 持续超前
            // （pts 相对锚点累计偏差超过 jump_reset 时还会触发重锚定）
            let arrival = t0 + Duration::from_millis(5 * i);
            s.push(frame(pts), arrival);
            pts = pts.wrapping_add(33);
            s.emit_due(arrival);
            s.drop_over_watermark(arrival);
            // 每次 drop 后缓冲跨度 ≤ target_delay：帧数受 target_delay/帧距 限制
            if s.queue.len() >= 2 {
                let extent = s
                    .queue
                    .back()
                    .unwrap()
                    .play_at
                    .saturating_duration_since(s.queue.front().unwrap().play_at);
                assert!(extent <= T, "缓冲跨度有界");
            }
            assert!(
                s.queue.len() <= (T.as_millis() / 33 + 2) as usize,
                "缓冲有界"
            );
        }
    }

    #[test]
    fn backwards_pts_jump_resets_buffer_and_reanchors() {
        let mut s = PlaybackScheduler::new(T, JUMP);
        let now = t0();
        s.push(frame(10000), now);
        assert_eq!(s.emit_due(now).len(), 1);
        // 向后大跳变（从 10000 跳到 0，差距 10000ms > 500ms JUMP）
        s.push(frame(0), now + Duration::from_millis(10));
        assert_eq!(s.stats.reanchors, 1, "向后大跳变应触发重新锚定");
        let out = s.emit_due(now + Duration::from_millis(10));
        assert_eq!(out.len(), 1, "新流首帧应立即发出");
        assert_eq!(out[0].pts_ms, 0);
    }

    #[test]
    fn emit_due_with_collects_frames() {
        let mut s = PlaybackScheduler::new(T, JUMP);
        let now = t0();
        s.push(frame(0), now);
        let mut emitted = Vec::new();
        let res: Result<(), ()> = s.emit_due_with(now, |f| {
            emitted.push(f.pts_ms);
            Ok(())
        });
        assert!(res.is_ok());
        assert_eq!(emitted, vec![0]);
    }
}
