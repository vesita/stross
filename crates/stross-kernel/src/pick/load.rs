//! 装载逻辑（发送侧）：把「标准化传输数据」装载为线上帧（打 id、加帧头）。
//!
//! 通信模式 v2（docs/comm-mode-v2.md §3.0）：pick 规则的发送侧镜像——
//! 与 [`interpret`](super::interpret)（解读逻辑）对称。内核提供装载框架，
//! 端点只做「原始数据 → 传输数据」的转化（编码/压缩/分块，经内核 trait）。
//!
//! 当前发送侧**行为等价直通**（`PassthroughLoader`）：采集/文件泵产出的
//! 帧已带全帧头（`Frame::to_bytes` 由 proto 统一打包），无额外缓冲需求
//! （plugin-architecture §9「不为抽象而抽象」）。Phase C 打 id / 调度发送
//! 节奏（StrictOrdered 按序完整 / Realtime 即时直通）在此扩展。

use stross_proto::frame::Frame;
use stross_proto::message::PickRule;

/// 装载逻辑：按 pick 规则把传输数据装载为可发送的线上帧。
///
/// 与 [`Interpreter`](super::Interpreter)（解读逻辑）两端对称，共用同一
/// [`PickRule`]。`Send` 帧 → 返回装载后的帧（当前实现直通）。
pub trait Loader: Send {
    /// 本装载器的 pick 规则。
    fn rule(&self) -> PickRule;
    /// 装载一帧（当前直通；Phase C 在此打 id / 调度）。
    fn load(&self, frame: Frame) -> Frame;
}

/// 直通装载器：帧原样通过（行为等价现状；StrictOrdered/Realtime 当前均直通）。
pub struct PassthroughLoader {
    rule: PickRule,
}

impl PassthroughLoader {
    pub fn new(rule: PickRule) -> Self {
        Self { rule }
    }
}

impl Loader for PassthroughLoader {
    fn rule(&self) -> PickRule {
        self.rule
    }

    fn load(&self, frame: Frame) -> Frame {
        frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stross_proto::frame::{CODEC_H264, Frame, TRACK_VIDEO};

    #[test]
    fn passthrough_loader_keeps_frame() {
        let f = Frame::new(TRACK_VIDEO, CODEC_H264, 0, 42, vec![1, 2, 3]);
        let loader = PassthroughLoader::new(PickRule::Realtime);
        assert_eq!(loader.rule(), PickRule::Realtime);
        let out = loader.load(f.clone());
        assert_eq!(out.header, f.header);
        assert_eq!(out.payload, f.payload);
    }
}
