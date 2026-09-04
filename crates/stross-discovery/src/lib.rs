//! **广播发现**：找到局域网节点及其端点。
//!
//! v3 概念定稿（docs/framework-v3.md §3.8）：发现服务只负责「找到并引导对接」，
//! 对接完成后数据连接建立与控制交给连接控制模块。
//!
//! * [`Discovery`]：发现服务契约（浏览 / 广播本机 / 单播探测）；
//! * [`MdnsDiscovery`]：mDNS 广播/浏览实现（本阶段内核直接调用具体类型，
//!   契约 trait 是未来统一面）；
//! * [`scan`] / [`scan_lan`] / [`probe_base`]：扫描聚合（mDNS 结果 + 子网单播
//!   回退 + HTTP 探测，收敛为 [`ScannedNode`] 列表）；
//! * [`ScannedNode`] / [`StreamView`]：发现聚合的展示视图（跨壳层消费）；
//! * [`DiscoveryEvent`]：节点上线/下线/端点变化事件（经内核聚合广播）。

mod mdns;
mod scan;

pub use mdns::{BROWSE_TIMEOUT, Discovered, MdnsDiscovery, SERVICE_TYPE};
pub use scan::{DISCOVERY_PORT, probe_base, scan, scan_lan};

use serde::{Deserialize, Serialize};

use stross_proto::message::{
    EndpointSummary, MediaKind, NodeId, RoleId, StreamId, StreamInfo, TransportId,
};

/// 发现聚合协议版本（随发现子系统契约封盘固定；子网单播扫描 / 统一发现清单
/// 的语义版本）。
pub const DISCOVERY_VERSION: &str = "0.2.0";

/// 单条流的展示视图（video/audio 布尔投影；`adb status` 复用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamView {
    pub stream_id: StreamId,
    pub title: String,
    pub video: bool,
    pub audio: bool,
    pub watchers: u32,
}

/// 流信息列表 → 展示视图。
pub fn to_views(list: Vec<StreamInfo>) -> Vec<StreamView> {
    list.into_iter()
        .map(|s| StreamView {
            stream_id: s.stream_id,
            title: s.title,
            video: s.video.is_some(),
            audio: s.audio.is_some(),
            watchers: s.watchers,
        })
        .collect()
}

/// 一个互联节点的聚合状态（发现 + 探测；跨壳层展示视图）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScannedNode {
    pub name: String,
    pub ip: String,
    pub port: u16,
    /// 是否本机（按本机局域网 IP 匹配）。
    pub is_self: bool,
    /// 角色（共享 / 接收 / 中继）。
    pub roles: Vec<RoleId>,
    /// 可共享媒体（屏幕 / 麦克风 …）。
    pub media: Vec<MediaKind>,
    /// 支持的传输（WS / SRT / QUIC …）。
    pub transports: Vec<TransportId>,
    /// 端点框架 L1：该节点公开的端点清单摘要（id/kind/name/是否可挂载/是否已通告）。
    pub endpoints: Vec<EndpointSummary>,
    /// `/api/info` 可达（HTTP 探测成功）才为 true。
    pub online: bool,
    pub srt_port: Option<u16>,
    pub quic_port: Option<u16>,
    /// 该设备当前在线共享（点流可在 GUI 接收）。
    pub streams: Vec<StreamView>,
}

/// 发现事件（节点上线 / 下线 / 端点变化；经内核聚合进 `stross_view::KernelEvent`）。
#[derive(Debug, Clone)]
pub enum DiscoveryEvent {
    NodeUp { node: NodeId },
    NodeDown { node: NodeId },
    EndpointsChanged { node: NodeId },
}

/// 发现服务：找到局域网节点及其端点。
///
/// * mDNS 实现（广播 + 浏览）、子网单播扫描实现都实现本契约；
/// * 内核持有 `Vec<Box<dyn Discovery>>`，事件聚合进内核事件面；
/// * 发现只负责「找到并引导对接」，不碰数据面。
pub trait Discovery: Send + Sync {
    /// 浏览：返回当前已知节点快照（含探测聚合结果）。
    fn browse(&self) -> Vec<ScannedNode>;
    /// 广播本机（开启/关闭「可被发现」；实现方自行管理生命周期）。
    fn advertise(&self, enabled: bool);
    /// 单播探测 / 手动地址加入；返回 `None` = 地址非法或不可达。
    fn probe(&self, addr: &str) -> Option<ScannedNode>;
}
