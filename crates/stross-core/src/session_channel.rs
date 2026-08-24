//! 交互数据管理模块（需求 docs/requirements.md §4.4 / F4）。
//!
//! 每条会话（session_id）两个通道：
//!
//! * **流式通道**（[`StreamChannel`]）：媒体帧。按会话协商出的传输可靠性分流——
//!   无损（WS/QUIC 全序不丢）直通；有损/自适应（WebRTC/SRT）经
//!   [抖动缓冲](crate::jitter::JitterBuffer) 按序 / 按关键帧对齐产出；
//! * **无损通道**（二期）：文件分块 / 剪贴板 / 输入，Lossless + 滑动窗口（另行实现）。
//!
//! [`SessionDataManager`] 是会话级容器：以 `session_id` 索引各通道，
//! 生命周期与会话一致（`channel()` 于会话建立、`remove()` 于会话拆除）。

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use stross_proto::frame::{Frame, TRACK_VIDEO};

use crate::jitter::{JitterBuffer, JitterConfig};

/// 流式通道的数据路径（由会话协商出的传输可靠性决定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    /// 无损（WS/QUIC）：全序不丢，直通不过抖动缓冲。
    Lossless,
    /// 有损/自适应（WebRTC/SRT）：可能乱序或缺帧，进抖动缓冲。
    Lossy,
}

/// 流式通道：按 `ChannelKind` 分流到直通队列或双轨抖动缓冲。
pub struct StreamChannel {
    kind: ChannelKind,
    /// 无损直通队列（有序到达，按序产出）。
    lossless_queue: VecDeque<Frame>,
    video: JitterBuffer,
    audio: JitterBuffer,
}

impl StreamChannel {
    /// 新建通道（kind 由会话协商结果决定）。
    pub fn new(kind: ChannelKind) -> Self {
        Self {
            kind,
            lossless_queue: VecDeque::new(),
            video: JitterBuffer::new(JitterConfig {
                require_keyframe_resync: true,
                ..JitterConfig::default()
            }),
            audio: JitterBuffer::new(JitterConfig {
                require_keyframe_resync: false,
                ..JitterConfig::default()
            }),
        }
    }

    /// 喂入一帧。
    pub fn push(&mut self, frame: Frame, now: Instant) {
        match self.kind {
            ChannelKind::Lossless => self.lossless_queue.push_back(frame),
            ChannelKind::Lossy => match frame.header.track {
                TRACK_VIDEO => self.video.push(frame, now),
                _ => self.audio.push(frame, now),
            },
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
    pub fn stats(&self) -> (crate::jitter::JitterStats, crate::jitter::JitterStats) {
        (self.video.stats, self.audio.stats)
    }
}

/// 交互数据管理：按会话索引流式通道。
#[derive(Default)]
pub struct SessionDataManager {
    channels: HashMap<String, StreamChannel>,
}

impl SessionDataManager {
    /// 会话建立时创建（或复用）其流式通道。
    pub fn channel(&mut self, session_id: &str, kind: ChannelKind) -> &mut StreamChannel {
        self.channels
            .entry(session_id.to_string())
            .or_insert_with(|| StreamChannel::new(kind))
    }

    /// 会话拆除时移除（释放缓冲）。
    pub fn remove(&mut self, session_id: &str) {
        self.channels.remove(session_id);
    }

    /// 当前活跃通道数。
    pub fn len(&self) -> usize {
        self.channels.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
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
    fn manager_lifecycle() {
        let mut m = SessionDataManager::default();
        assert!(m.is_empty());
        m.channel("sess-1", ChannelKind::Lossy)
            .push(frame(TRACK_AUDIO, 0, false), Instant::now());
        m.channel("sess-2", ChannelKind::Lossless);
        assert_eq!(m.len(), 2);
        m.remove("sess-1");
        assert_eq!(m.len(), 1);
        assert!(m.channels.contains_key("sess-2"));
    }
}
