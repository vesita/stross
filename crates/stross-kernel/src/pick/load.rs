//! 装载逻辑（发送侧）：把「标准化传输数据」装载为线上帧（打 id、加帧头）。
//!
//! 通信模式 v2（docs/comm-mode-v2.md §3.0）：pick 规则的发送侧镜像——
//! 与 [`interpret`](super::interpret)（解读逻辑）对称。**序列化规则是内核/
//! 协议工具（数据契约）**：分享端点与订阅端点之间按策略 id 传输数据包，
//! 装载（本模块）与解读（interpret）都由内核按 [`EndpointStrategy`]（序列化
//! 规则 + pick 规则）装载——端点只声明策略，不实现序列化。
//!
//! 当前发送侧**行为等价直通**（[`PassthroughLoader`]）：采集/文件泵产出的
//! 帧已带全帧头（`Frame::to_bytes` 由 proto 统一打包），无额外缓冲需求
//! （plugin-architecture §9「不为抽象而抽象」）。`SerializeRule::Chunked`
//! （分包）为预留规则——`for_strategy` 对未实现的规则返回明确错误，
//! 协商/订阅边界据此拒绝，避免"grant 成功但数据契约不匹配"。

use stross_proto::frame::Frame;
use stross_proto::message::{EndpointStrategy, PickRule, SerializeRule};

/// 装载逻辑：按策略（序列化规则 + pick 规则）把传输数据装载为可发送的线上帧。
///
/// 与 [`Interpreter`](super::Interpreter)（解读逻辑）两端对称，共用同一
/// [`EndpointStrategy`]。`Send` 帧 → 返回装载后的帧（当前实现直通）。
pub trait Loader: Send {
    /// 本装载器的 pick 规则。
    fn rule(&self) -> PickRule;
    /// 本装载器实现的序列化规则（数据契约；`Passthrough` 当前唯一实现）。
    fn serialize_rule(&self) -> SerializeRule;
    /// 装载一帧（当前直通；Phase C 在此打 id / 调度 / 分包）。
    fn load(&self, frame: Frame) -> Frame;
}

/// 按策略装载装载器（内核序列化工具，分享/订阅两端共用）：
/// 未实现的序列化规则返回 `None`（调用方拒绝，不做静默降级）。
pub fn loader_for(strategy: &EndpointStrategy) -> Option<Box<dyn Loader>> {
    match strategy.serialize {
        SerializeRule::Passthrough => Some(Box::new(PassthroughLoader::new(
            strategy.serialize,
            strategy.pick,
        ))),
        SerializeRule::Chunked => None, // 分包装载器预留：无端点声明，未实现
    }
}

/// 直通装载器：帧原样通过（行为等价现状；StrictOrdered/Realtime 当前均直通）。
pub struct PassthroughLoader {
    serialize: SerializeRule,
    rule: PickRule,
}

impl PassthroughLoader {
    pub fn new(serialize: SerializeRule, rule: PickRule) -> Self {
        Self { serialize, rule }
    }
}

impl Loader for PassthroughLoader {
    fn rule(&self) -> PickRule {
        self.rule
    }

    fn serialize_rule(&self) -> SerializeRule {
        self.serialize
    }

    fn load(&self, frame: Frame) -> Frame {
        frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_loader_keeps_frame() {
        let f = Frame::new(
            stross_proto::frame::TRACK_VIDEO,
            stross_proto::frame::CODEC_H264,
            0,
            42,
            vec![1, 2, 3],
        );
        let loader = PassthroughLoader::new(SerializeRule::Passthrough, PickRule::Realtime);
        assert_eq!(loader.rule(), PickRule::Realtime);
        assert_eq!(loader.serialize_rule(), SerializeRule::Passthrough);
        let out = loader.load(f.clone());
        assert_eq!(out.header, f.header);
        assert_eq!(out.payload, f.payload);
    }

    /// 序列化 = 内核数据契约：for_strategy 按序列化规则装载，未实现规则拒绝
    /// （不静默降级——协商/订阅边界据此拒绝 grant）。
    #[test]
    fn loader_for_strategy_dispatch() {
        let passthrough = EndpointStrategy::passthrough(PickRule::Realtime);
        let loader = loader_for(&passthrough).expect("Passthrough 应可装载");
        assert_eq!(loader.serialize_rule(), SerializeRule::Passthrough);

        let chunked = EndpointStrategy {
            strategy_id: "chunked".into(),
            serialize: SerializeRule::Chunked,
            pick: PickRule::StrictOrdered,
        };
        assert!(
            loader_for(&chunked).is_none(),
            "Chunked 分包装载器未实现 → 拒绝（不静默降级）"
        );
    }
}
