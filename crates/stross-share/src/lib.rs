//! **共享**（内容源侧编排）：发布端点 → 管理共享生命周期。
//!
//! v3 概念定稿（docs/framework-v3.md §3.3）：共享 = 我是内容源（推送预备）。
//! 内容源把端点发布为「可被订阅」，订阅达成后端点自驱动推流；生命周期
//! 治理（watchers=0 自动收尾 / 取消通告联动停止）由本契约承载。
//!
//! 从 kernel 的 `active_shares` / `note_share_active` / `stop_share_if_unwatched`
//! 逻辑抽提为契约；内核实现 [`ShareService`]（持有 stream → ActiveShare 映射 +
//! watchers 治理），端点只依赖该 trait 回调节点。

use stross_endpoint::{ShareEndpoint, SubscribeCtx};
use stross_proto::message::{Delivery, EndpointId, NodeId, StreamId, Visibility};

/// 共享句柄（发布 / 撤销的操作凭据；`StreamId` 即数据面流 id）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ShareHandle(pub StreamId);

/// 共享登记条目（实时目标生命周期治理；文件端点有完成态，不登记）。
#[derive(Debug, Clone)]
pub struct ActiveShare {
    pub endpoint_id: EndpointId,
    pub delivery: Delivery,
    /// 订阅者节点集（显式订阅终止通知用：最后一个订阅者离开即收敛）。
    pub subscriber_nodes: Vec<NodeId>,
}

/// 内容源侧编排：发布端点 → 管理共享生命周期。
///
/// 内核实现本契约（持有共享登记表 + watchers 治理任务）；端点层只经
/// [`ShareEndpoint`] 契约被调用，不依赖内核具体类型。
pub trait ShareService: Send + Sync {
    /// 发布一个端点为「可被订阅」（含可见性策略；不可挂载端点拒绝）。
    fn publish(
        &self,
        ep: &dyn ShareEndpoint,
        visibility: Visibility,
    ) -> Result<ShareHandle, String>;
    /// 撤销发布（端点保留在注册表，可再次通告）。
    fn unpublish(&self, handle: &ShareHandle) -> Result<(), String>;
    /// 订阅达成回调：登记共享 + 端点自驱动 `share`（内核不分派类型）。
    fn on_subscribed(&self, ep: &dyn ShareEndpoint, ctx: &SubscribeCtx, endpoint_id: EndpointId);
    /// 生命周期治理：watchers 归零复查确认无人观看后停止共享（默认延迟由
    /// 实现方决定；给订阅者重连 / 新订阅者接入窗口）。
    fn reap_if_unwatched(&self, stream: &StreamId);
    /// 显式停止端点共享（保留通告；同端点订阅收敛 / 取消通告联动）。
    fn stop(&self, endpoint_id: EndpointId) -> Result<(), String>;
    /// 当前活动共享快照（stream → 共享登记）。
    fn active(&self) -> Vec<(StreamId, ActiveShare)>;
}
