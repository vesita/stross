//! 直通装载器（`SerializeRule::Passthrough`）：原始数据 → 单帧，原样打包。
//!
//! 序列化 = 内核数据契约（docs/framework-v3.md §3.6）：分享端点与订阅端点
//! 之间按策略 id 传输数据包，装载（本模块）与解装载（`Unloader`）都由内核
//! 按 [`EndpointStrategy`]（序列化规则 + pick 规则）装载——端点只声明策略，
//! 不实现序列化。
//!
//! 当前唯一实现 `Passthrough`（直通）：`load` 把「原始数据」打包成**单帧**
//! （帧头轨道/编解码字节取自 `TrackInfo`，pts 透传）。`SerializeRule::Chunked`
//! （分包）为预留规则——[`loader_for`] 对未实现的规则返回 `None`（调用方
//! 拒绝），避免"grant 成功但数据契约不匹配"（不静默降级）。

use stross_proto::frame::{CODEC_AAC, CODEC_H264, Frame, TRACK_AUDIO, TRACK_VIDEO};
use stross_proto::message::{CodecId, EndpointStrategy, PickRule, SerializeRule, TrackInfo};

use crate::Loader;

/// 直通装载器：把「原始数据」打包为单帧（当前唯一实现；分包规则在装载器内扩展）。
pub struct PassthroughLoader {
    serialize: SerializeRule,
    rule: PickRule,
}

impl PassthroughLoader {
    pub fn new(serialize: SerializeRule, rule: PickRule) -> Self {
        Self { serialize, rule }
    }

    /// 本装载器的 pick 规则（装载端保留档案，与解读端对称，供调用方确认策略）。
    pub fn rule(&self) -> PickRule {
        self.rule
    }
}

impl Loader for PassthroughLoader {
    fn serialize_rule(&self) -> SerializeRule {
        self.serialize
    }

    /// 装载一个数据包为单帧：帧头轨道/编解码字节由 [`TrackInfo`] 推导，
    /// `pts_ms` 透传进帧头；返回一帧（未实现编解码返回空，不伪造 wire 字节）。
    fn load(&self, track: TrackInfo, data: &[u8], pts_ms: u32) -> Vec<Frame> {
        match track_codec_bytes(track.codec) {
            // 拷贝进 `Bytes`（帧载荷自有所有权，不借用调用方缓冲区）
            Some((track, codec)) => vec![Frame::new(track, codec, 0, pts_ms, data.to_vec())],
            None => Vec::new(),
        }
    }
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

/// `TrackInfo` 的编解码 → 帧头轨道/编解码字节（wire 编解码字节单一真源：
/// docs/protocol.md「1=H.264(Annex-B)，2=AAC(ADTS)」；Opus/Av1 预留尚无
/// wire 字节 → 不可装载）。
fn track_codec_bytes(codec: CodecId) -> Option<(u8, u8)> {
    match codec {
        CodecId::H264 => Some((TRACK_VIDEO, CODEC_H264)),
        CodecId::Aac => Some((TRACK_AUDIO, CODEC_AAC)),
        CodecId::Opus | CodecId::Av1 => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stross_proto::frame::{CODEC_H264, TRACK_VIDEO};

    fn track(codec: CodecId) -> TrackInfo {
        TrackInfo {
            codec,
            width: None,
            height: None,
            fps: None,
            sample_rate: None,
            channels: None,
        }
    }

    #[test]
    fn passthrough_loader_packs_single_frame() {
        let loader = PassthroughLoader::new(SerializeRule::Passthrough, PickRule::Realtime);
        assert_eq!(loader.rule(), PickRule::Realtime);
        assert_eq!(loader.serialize_rule(), SerializeRule::Passthrough);
        let out = loader.load(track(CodecId::H264), &[1, 2, 3], 42);
        assert_eq!(out.len(), 1);
        let f = &out[0];
        assert_eq!(f.header.track, TRACK_VIDEO);
        assert_eq!(f.header.codec, CODEC_H264);
        assert_eq!(f.header.pts_ms, 42);
        assert_eq!(f.payload.as_ref(), &[1, 2, 3]);
    }

    /// 预留编解码（无 wire 字节）不装载：返回空，不伪造帧头。
    #[test]
    fn reserved_codecs_not_loaded() {
        let loader = PassthroughLoader::new(SerializeRule::Passthrough, PickRule::Realtime);
        assert!(loader.load(track(CodecId::Opus), &[1], 0).is_empty());
        assert!(loader.load(track(CodecId::Av1), &[1], 0).is_empty());
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
