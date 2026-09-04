//! **订阅**（接收方侧编排）：订阅端点 → 建链 → 呈现。
//!
//! v3 概念定稿（docs/framework-v3.md §3.4）：订阅 = 我是接收方（pull 驱动）。
//! 数据流一律由订阅方发起并主动取（pull）——共享方只在**自己的**受控中继
//! 发布，订阅方连共享方中继 watch；取消 push（共享方不主动出站推送）。
//!
//! 从 kernel 的 `subscriber` / `receiver` / `generate_subscribe_endpoint`
//! 编排逻辑抽提为契约；内核实现本契约，壳层/端点只消费。

use stross_endpoint::SubscribeEndpoint;
use stross_proto::message::{EndpointId, LinkId, NodeId, StrategyId, StreamId, SubscribeSpec};

/// 订阅链路条目（多端点链接：一次可同时接收多条流，互不级联）。
#[derive(Debug, Clone)]
pub struct SubscribeLink {
    pub link_id: LinkId,
    pub node_id: NodeId,
    pub endpoint_id: EndpointId,
    pub stream_id: StreamId,
    pub running: bool,
}

/// 接收方侧编排：订阅端点 → 建链 → 呈现。
///
/// 内核实现本契约（注册表查表 + 订阅端点生成 + 链路启停/统计）；订阅端点
/// （播放器 / 文件接收）只经 [`SubscribeEndpoint`] 契约被调用。
pub trait SubscribeService: Send + Sync {
    /// 解析 `(节点, 端点, 策略)` → 订阅规格（注册表统一查表；未知返回 `None`）。
    fn resolve(
        &self,
        node: &NodeId,
        endpoint: EndpointId,
        strategy: Option<&StrategyId>,
    ) -> Option<SubscribeSpec>;
    /// 建链并交给订阅端点处理（媒体播放 / 文件落盘）。
    fn subscribe(
        &self,
        spec: SubscribeSpec,
        sink: Box<dyn SubscribeEndpoint>,
    ) -> Result<LinkId, String>;
    /// 停止一条订阅链路（多链路互不级联）。
    fn unsubscribe(&self, link: &LinkId) -> Result<(), String>;
    /// 全部链路快照。
    fn links(&self) -> Vec<SubscribeLink>;
}
