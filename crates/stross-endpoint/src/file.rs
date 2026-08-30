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
    Delivery, EndpointStrategy, MediaKind, PickRule, ReliabilityProfile, SerializeRule,
    SubscribeSpec,
};

use crate::contract::{Endpoint, EndpointApp, EndpointBase, SubscribeCtx, TargetKind};

/// 文件泵参数（公开方驱动构造；内核 `push_file` 消费——契约单一真源）。
#[derive(Debug, Clone)]
pub struct FilePushOptions {
    /// 中继推流地址（`ws://host:port/ws/push`；文件走无损 WS 路径）。
    pub push_url: String,
    /// 数据面流 id（pull = 公开方本机会话；push = 订阅方自签会话）。
    pub stream_id: String,
    /// 推流标题（Hello.title；展示用）。
    pub title: String,
    /// 跨设备接入凭证（push 模式 = 订阅方自签；本机 pull = `None`）。
    pub share_token: Option<String>,
    /// 观看数轮询基址（`ws://host:port`；`None` = 不等观看者直接推）。
    pub watcher_base: Option<String>,
}

/// 文件端点（确定目标）。
pub struct FileEndpoint {
    base: EndpointBase,
    path: PathBuf,
}

impl FileEndpoint {
    pub const fn new(endpoint_id: String, name: String, path: PathBuf) -> Self {
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
    fn id(&self) -> &str {
        &self.base.id
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

/// 文件**订阅端**端点（v2 双向能力体的接收侧；docs/endpoint-model-v2.md §3）：
/// 由内核从注册表 `(节点, 端点, 策略)` 解析后**生成**（订阅端点生成），
/// 经 [`EndpointApp::receive_file`] 把订阅的文件流落盘到 `out_dir`。
///
/// 只作订阅端（`supports_subscribe() == true`；`share` 不应被调用——本端点
/// 不进入通告/目录，仅订阅编排内部构造）。
pub struct FileReceiveEndpoint {
    base: EndpointBase,
    out_dir: PathBuf,
}

impl FileReceiveEndpoint {
    /// `out_dir`：接收落盘目录（订阅方用户意图；订阅端点生成时注入）。
    pub const fn new(endpoint_id: String, name: String, out_dir: PathBuf) -> Self {
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
    fn id(&self) -> &str {
        &self.base.id
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
    fn available(&self) -> bool {
        self.base.available
    }
    fn last_error(&self) -> Option<&str> {
        self.base.last_error.as_deref()
    }
    fn load(&mut self) -> StdResult<(), String> {
        // 接收端点不探测源可用性（落盘目录由 receive_file 创建）
        Ok(())
    }
    fn share(&self, _app: Arc<dyn EndpointApp>, _ctx: SubscribeCtx) {
        tracing::warn!(
            "文件订阅端端点 {} 不应被分享（仅订阅端），忽略 share",
            self.id()
        );
    }
    fn supports_subscribe(&self) -> bool {
        true
    }
    fn subscribe(&self, app: Arc<dyn EndpointApp>, spec: SubscribeSpec) {
        let out_dir = self.out_dir.clone();
        let endpoint_id = self.id().to_string();
        tokio::spawn(async move {
            let Some(watch_url) = spec.relay_url.clone() else {
                tracing::warn!("文件订阅端 {endpoint_id} 缺公开方中继地址（pull 未锚定）");
                return;
            };
            match app
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
        });
    }
}
