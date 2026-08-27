//! 设备图：节点注册与能力聚合。

use std::collections::HashMap;
use std::sync::Mutex;

use serde::Serialize;

use stross_proto::message::CapabilityDescriptor;

/// 节点角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum NodeRole {
    Sender,
    Viewer,
    Relay,
    Controller,
}

/// 节点可达传输地址（传输 + 地址；设备图内的路由条目）。
///
/// 与「端点框架」的端点（docs/endpoint-model.md：设备被公开后的订阅入口）
/// **不是同一概念**——本结构只是图内一条「怎么拨到这个节点」的记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransportAddr {
    pub transport: String,
    pub addr: String,
}

/// 一个参与互联的节点。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeInfo {
    pub node_id: String,
    pub name: String,
    pub roles: Vec<NodeRole>,
    pub caps: Vec<CapabilityDescriptor>,
    pub addrs: Vec<TransportAddr>,
}

/// 设备图：节点注册与能力聚合。
#[derive(Default)]
pub(super) struct DeviceGraph {
    pub(super) nodes: Mutex<HashMap<String, NodeInfo>>,
}
