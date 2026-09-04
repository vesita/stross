//! **节点**（网络拓扑主体）= **端点插件的宿主**：手机 / 电脑 / 中继等互联实体。
//!
//! v3 概念定稿（docs/framework-v3.md §3.1 / §10.4）：节点是上层实体，**拥有多个
//! 端点**（下属）；节点只承载「身份 + 角色 + 能力 + 可达地址」，**不承载共享/
//! 订阅方向**（方向在端点层）。节点可以是本机（`LocalNode`）或远端（发现/目录
//! 映射）。
//!
//! 端点（`stross-endpoint` 的 `Endpoint` 契约实现）是**挂载到节点插件区的插件**：
//! 节点 = 插件宿主，插件经 `load` 探测可挂载性、经四能力 trait（`StreamHost` /
//! `FileHost` / `MediaHost` / `Runtime`）调用宿主的调度能力。**插件清单视图是
//! 内核注册表的查询投影，不属于节点行为**——`Node` trait 只声明节点自身的拓扑
//! 身份与能力（id / name / roles / caps / addrs），无 `endpoints()` / `plugins()`。
//!
//! 依赖方向：`stross-node → stross-proto`（线协议纯类型，唯一底层依赖）。

use serde::Serialize;

use stross_proto::message::{CapabilityDescriptor, NodeId};

/// 节点角色（发现广播用；有限集合）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum NodeRole {
    /// 可作源（推流）。
    Sender,
    /// 可作汇（接收播放）。
    Viewer,
    /// 中继（转发数据面）。
    Relay,
    /// 控制者（控制面；D7 远程控制阶段开放）。
    Controller,
}

/// 节点可达传输地址（传输 + 地址；节点图内的路由条目）。
///
/// 与「端点框架」的端点（docs/framework-v3.md §3.2：内容被公开后的订阅入口）
/// **不是同一概念**——本结构只是图内一条「怎么拨到这个节点」的记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransportAddr {
    pub transport: String,
    pub addr: String,
}

/// 一个参与互联的节点（拓扑快照：身份 + 角色 + 能力 + 可达地址）。
///
/// 与 [`Node`] trait 的关系：本结构是**节点视图/DTO**（发现/图聚合产物，
/// 跨壳层展示用），同时**直接实现行为契约**（「注册表条目即实现」——见下方
/// `impl Node for NodeInfo`，五个方法逐字段投影）；[`Node`] 是**行为契约**
/// （内核 `upsert_node` 接纳任意节点形态的抽象面）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeInfo {
    pub node_id: NodeId,
    pub name: String,
    pub roles: Vec<NodeRole>,
    pub caps: Vec<CapabilityDescriptor>,
    pub addrs: Vec<TransportAddr>,
}

/// 节点行为契约：网络拓扑中的互联主体 = **端点插件的宿主**。
///
/// * 本机节点与远端节点实现同一 trait（注册表/图持有 `Box<dyn Node>`）；
/// * 节点**不承载共享/订阅方向**——方向在端点层（`stross-endpoint`）；
/// * **无 `endpoints()` / `plugins()`**：插件区清单是内核注册表的查询投影，
///   不属节点行为契约（docs/framework-v3.md §10.4）。
pub trait Node: Send + Sync {
    /// 节点全局拓扑标识。
    fn id(&self) -> NodeId;
    /// 展示名。
    fn name(&self) -> &str;
    /// 角色（可作源 / 可作汇 / 中继 / 控制者）。
    fn roles(&self) -> &[NodeRole];
    /// 能力描述（发现/协商用）。
    fn caps(&self) -> &[CapabilityDescriptor];
    /// 可达地址（传输候选）。
    fn addrs(&self) -> &[TransportAddr];
}

/// 视图 DTO 实现行为契约（「注册表条目即实现」）：五个方法逐字段投影。
impl Node for NodeInfo {
    fn id(&self) -> NodeId {
        self.node_id
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn roles(&self) -> &[NodeRole] {
        &self.roles
    }
    fn caps(&self) -> &[CapabilityDescriptor] {
        &self.caps
    }
    fn addrs(&self) -> &[TransportAddr] {
        &self.addrs
    }
}
