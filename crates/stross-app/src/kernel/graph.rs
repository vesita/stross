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

/// 节点能力端点（传输 + 地址）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Endpoint {
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
    pub endpoints: Vec<Endpoint>,
}

/// 设备图：节点注册与能力聚合。
#[derive(Default)]
pub(super) struct DeviceGraph {
    pub(super) nodes: Mutex<HashMap<String, NodeInfo>>,
}
