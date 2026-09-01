//! 媒体订阅端点（v3 能力族：Graph / Audio 类的**统一订阅端**，播放器入端点）。
//!
//! 由内核按注册表 `(节点, 端点, 策略)` 解析后**生成**（订阅端点生成），
//! 订阅时经 [`EndpointApp::receive_media`] 连公开方中继收流、按订阅规格的
//! pick 规则解读并解码——**视频播放器 / 音频播放这类"数据还原"被纳入端点
//! 概念**：Graph 类（屏幕/窗口/摄像头）与 Audio 类（麦克风/系统声音）共用
//! 本实现，不再逐个端点写订阅样板。
//!
//! 订阅端与分享端是**独立契约**（[`SubscribeEndpoint`]，docs/endpoint-model-v2.md
//! §3 演进）——本端点只有 `subscribe`，无分享占位；不进通告/目录，仅订阅
//! 编排内部构造。文件类的订阅端见 [`crate::subscribe::file::FileReceiveEndpoint`]。

use std::sync::Arc;

use stross_proto::message::{
    EndpointId, EndpointStrategy, MediaKind, PickRule, ReliabilityProfile, SerializeRule,
    SubscribeSpec,
};

use crate::contract::{Endpoint, EndpointApp, EndpointBase, TargetKind};

/// 媒体订阅端点（Graph / Audio 类统一订阅端）。
pub struct MediaReceiveEndpoint {
    base: EndpointBase,
}

impl MediaReceiveEndpoint {
    /// `kind`：订阅目标的内容类型（屏幕/麦克风/…；能力族由 kind 推导——
    /// 同一族共享本实现）。
    pub const fn new(endpoint_id: EndpointId, name: String, kind: MediaKind) -> Self {
        Self {
            base: EndpointBase {
                id: endpoint_id,
                kind,
                name,
                available: true,
                last_error: None,
            },
        }
    }
}

impl Endpoint for MediaReceiveEndpoint {
    fn id(&self) -> EndpointId {
        self.base.id
    }
    fn kind(&self) -> MediaKind {
        self.base.kind
    }
    fn name(&self) -> &str {
        &self.base.name
    }
    fn target(&self) -> TargetKind {
        TargetKind::Live
    }
    fn transport_profile(&self) -> ReliabilityProfile {
        // 实时媒体：允许丢包（接收侧按 pick 规则解读）
        ReliabilityProfile::Lossy
    }
    fn strategy(&self) -> EndpointStrategy {
        // 订阅端自身不分享；策略默认值与媒体源一致（直通 + 严格即时）
        EndpointStrategy {
            strategy_id: EndpointStrategy::DEFAULT_ID.into(),
            serialize: SerializeRule::Passthrough,
            pick: PickRule::Realtime,
        }
    }
}

impl crate::contract::SubscribeEndpoint for MediaReceiveEndpoint {
    fn subscribe(&self, app: Arc<dyn EndpointApp>, spec: SubscribeSpec) {
        let endpoint_id = self.id().to_string();
        tokio::spawn(async move {
            match app.receive_media(&spec).await {
                Ok(frames) => tracing::info!(
                    "媒体订阅端点 {endpoint_id} 接收完成（节点 {}，端点 {}，策略 {}，解码 {frames} 帧）",
                    spec.node_id,
                    EndpointId::new(spec.kind, spec.endpoint_id),
                    spec.strategy.strategy_id,
                ),
                Err(e) => tracing::warn!(
                    "媒体订阅端点 {endpoint_id} 接收失败（端点 {}）: {e:#}",
                    EndpointId::new(spec.kind, spec.endpoint_id),
                ),
            }
        });
    }
}
