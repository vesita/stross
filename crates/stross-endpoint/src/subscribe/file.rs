//! 文件**订阅端点**（File 能力族的订阅端；docs/endpoint-model-v2.md §3）：
//! 由内核从注册表 `(节点, 端点, 策略)` 解析后**生成**（订阅端点生成），
//! 经 [`EndpointApp::receive_file`] 把订阅的文件流落盘到 `out_dir`。
//!
//! 订阅端与分享端是**独立契约**（[`SubscribeEndpoint`]）——本端点只有
//! `subscribe`，无分享占位；不进通告/目录，仅订阅编排内部构造。

use std::path::PathBuf;
use std::sync::Arc;

use stross_proto::message::{
    EndpointId, EndpointStrategy, MediaKind, PickRule, ReliabilityProfile, SubscribeSpec,
};

use crate::contract::{Endpoint, EndpointApp, EndpointBase, TargetKind};

pub struct FileReceiveEndpoint {
    base: EndpointBase,
    out_dir: PathBuf,
}

impl FileReceiveEndpoint {
    /// `out_dir`：接收落盘目录（订阅方用户意图；订阅端点生成时注入）。
    pub const fn new(endpoint_id: EndpointId, name: String, out_dir: PathBuf) -> Self {
        Self {
            base: EndpointBase {
                id: endpoint_id,
                kind: MediaKind::File,
                name,
                available: true,
                last_error: None,
            },
            out_dir,
        }
    }
}

impl Endpoint for FileReceiveEndpoint {
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
        // 文件确定目标：逐字节不丢，走无损
        ReliabilityProfile::Lossless
    }
    fn strategy(&self) -> EndpointStrategy {
        // 确定目标：直通序列化 + 严格顺序（StrictOrdered）
        EndpointStrategy::passthrough(PickRule::StrictOrdered)
    }
}

impl crate::contract::SubscribeEndpoint for FileReceiveEndpoint {
    fn subscribe(&self, app: Arc<dyn EndpointApp>, spec: SubscribeSpec) {
        let out_dir = self.out_dir.clone();
        let endpoint_id = self.id().to_string();
        // 端点自驱动统一经 `EndpointApp::spawn_task`（与分享端 `share` 同构，
        // docs/endpoint-model-v2.md §3：运行时由内核注入）。
        let app2 = app.clone();
        app.spawn_task(Box::pin(async move {
            let Some(watch_url) = spec.relay_url.clone() else {
                tracing::warn!("文件订阅端 {endpoint_id} 缺公开方中继地址（pull 未锚定）");
                return;
            };
            match app2
                .receive_file(watch_url, spec.stream_id.clone(), out_dir)
                .await
            {
                Ok(r) => tracing::info!(
                    "文件订阅端 {endpoint_id} 已接收「{}」({} 字节, stream={}, 策略={}) → {}",
                    r.name,
                    r.size,
                    spec.stream_id,
                    spec.strategy.strategy_id,
                    r.path.display(),
                ),
                Err(e) => tracing::warn!(
                    "文件订阅端 {endpoint_id} 接收失败（stream={}）: {e:#}",
                    spec.stream_id
                ),
            }
        }));
    }
}
