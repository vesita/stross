//! 文件通道端点（双向对等信道）：load 恒可用；
//! 订阅达成后作为被连接方等待/配合对端建立全双工文件与消息通道。

use std::result::Result as StdResult;
use std::sync::Arc;

use stross_proto::message::{
    EndpointId, EndpointStrategy, MediaKind, PickRule, ReliabilityProfile,
};

use crate::contract::{
    Endpoint, EndpointApp, EndpointBase, ShareEndpoint, SubscribeCtx, TargetKind,
};

/// 节点文件与消息互传通道端点（分享侧）。
pub struct FileChannelEndpoint {
    base: EndpointBase,
}

impl FileChannelEndpoint {
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

impl Endpoint for FileChannelEndpoint {
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
        // 双向通道严格无损传输
        ReliabilityProfile::Lossless
    }
    fn strategy(&self) -> EndpointStrategy {
        // 严格顺序无损交付
        EndpointStrategy::passthrough(PickRule::StrictOrdered)
    }
}

impl ShareEndpoint for FileChannelEndpoint {
    fn available(&self) -> bool {
        self.base.available
    }
    fn last_error(&self) -> Option<&str> {
        self.base.last_error.as_deref()
    }
    fn load(&mut self) -> StdResult<(), String> {
        self.base.available = true;
        self.base.last_error = None;
        Ok(())
    }
    fn share(&self, _app: Arc<dyn EndpointApp>, _ctx: SubscribeCtx) {
        // 服务端接入由中继 /ws/channel 配合内核 ChannelManager 驱动
    }
}
