//! 文件通道订阅端点（双向对等信道的订阅侧）：
//! 由内核从注册表 `(节点, 端点, 策略)` 解析后生成，配合内核 ChannelManager 发起全双工通道。

use std::sync::Arc;

use stross_proto::message::{
    EndpointId, EndpointStrategy, MediaKind, PickRule, ReliabilityProfile, SubscribeSpec,
};

use crate::contract::{Endpoint, EndpointApp, EndpointBase, SubscribeEndpoint, TargetKind};

/// 节点文件与消息互传通道订阅端点。
pub struct FileChannelSubscribeEndpoint {
    base: EndpointBase,
}

impl FileChannelSubscribeEndpoint {
    pub const fn new(endpoint_id: EndpointId, name: String) -> Self {
        Self {
            base: EndpointBase {
                id: endpoint_id,
                kind: MediaKind::File,
                name,
                available: true,
                last_error: None,
            },
        }
    }
}

impl Endpoint for FileChannelSubscribeEndpoint {
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
        TargetKind::Determined
    }
    fn transport_profile(&self) -> ReliabilityProfile {
        ReliabilityProfile::Lossless
    }
    fn strategy(&self) -> EndpointStrategy {
        EndpointStrategy::passthrough(PickRule::StrictOrdered)
    }
}

impl SubscribeEndpoint for FileChannelSubscribeEndpoint {
    fn subscribe(&self, app: Arc<dyn EndpointApp>, spec: SubscribeSpec) {
        let endpoint_id = self.id();
        app.spawn_task(Box::pin(async move {
            tracing::info!(
                "文件通道订阅端点 {endpoint_id} 已就绪: stream={}, 策略={}",
                spec.stream_id,
                spec.strategy.strategy_id
            );
        }));
    }
}
