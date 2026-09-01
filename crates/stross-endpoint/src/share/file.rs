//! 文件端点（确定目标）：load 探测文件可读；share 一次性推送（传完回 Idle）。
//! **双向能力体**（docs/endpoint-model-v2.md §3）：分享端推文件（[`FileEndpoint`]）、
//! 订阅端接收落盘（[`FileReceiveEndpoint`]）——方向挂载在端点层。
//!
//! 本地路径只存在于端点对象内（**绝不出现在 wire / 目录 / 摘要**）。
//! 文件泵执行（push_file）是内核调度能力，经 [`crate::contract::EndpointApp::push_file`]
//! 调用；订阅端接收经 [`crate::contract::EndpointApp::receive_file`] 落盘；
//! `FilePushOptions` 是两端共用的纯数据契约（单一真源在此）。

use std::path::PathBuf;
use std::result::Result as StdResult;
use std::sync::Arc;

use stross_proto::message::{
    Delivery, EndpointId, EndpointStrategy, MediaKind, PickRule, ReliabilityProfile, SerializeRule,
};

use crate::contract::{
    Endpoint, EndpointApp, EndpointBase, ShareEndpoint, SubscribeCtx, TargetKind,
};

/// 文件泵参数（契约单一真源在 stross-types）；此处重导出保持
/// `stross_endpoint::file::FilePushOptions` 路径兼容。
pub use stross_types::contract::FilePushOptions;

/// 文件端点（确定目标）。
pub struct FileEndpoint {
    base: EndpointBase,
    path: PathBuf,
}

impl FileEndpoint {
    pub const fn new(endpoint_id: EndpointId, name: String, path: PathBuf) -> Self {
        Self {
            base: EndpointBase {
                id: endpoint_id,
                kind: MediaKind::File,
                name,
                available: false,
                last_error: None,
            },
            path,
        }
    }
}

impl Endpoint for FileEndpoint {
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
        EndpointStrategy {
            strategy_id: EndpointStrategy::DEFAULT_ID.into(),
            serialize: SerializeRule::Passthrough,
            pick: PickRule::StrictOrdered,
        }
    }
}

impl ShareEndpoint for FileEndpoint {
    fn available(&self) -> bool {
        self.base.available
    }
    fn last_error(&self) -> Option<&str> {
        self.base.last_error.as_deref()
    }
    fn load(&mut self) -> StdResult<(), String> {
        if self.path.is_file() {
            self.base.available = true;
            self.base.last_error = None;
            Ok(())
        } else {
            let e = format!("文件不可读: {}", self.path.display());
            self.base.mark_failed(e.clone());
            Err(e)
        }
    }
    fn share(&self, app: Arc<dyn EndpointApp>, ctx: SubscribeCtx) {
        let path = self.path.clone();
        let name = self.name().to_string();
        let endpoint_id = self.id().to_string();
        tokio::spawn(async move {
            let Some(url) = crate::contract::resolve_file_url(app.as_ref(), &ctx) else {
                tracing::warn!(
                    "文件端点 {endpoint_id} 无可用推送地址（pull 未锚定中继 / push 缺订阅方地址）"
                );
                return;
            };
            let watcher_base = crate::contract::resolve_watcher_base(app.as_ref(), &ctx);
            let opts = FilePushOptions {
                push_url: url,
                stream_id: ctx.stream_id.clone(),
                title: format!("文件 {name}"),
                share_token: if ctx.delivery == Delivery::Push {
                    ctx.share_token.clone()
                } else {
                    None
                },
                watcher_base,
            };
            match app.push_file(path, opts).await {
                Ok(sent) => tracing::info!(
                    "文件端点 {endpoint_id} 已推送「{name}」({sent} 字节, stream={}) 给订阅方 {}",
                    ctx.stream_id,
                    ctx.subscriber,
                ),
                Err(e) => tracing::warn!(
                    "文件端点 {endpoint_id} 推送失败（订阅方 {}）: {e:#}",
                    ctx.subscriber
                ),
            }
        });
    }
}
