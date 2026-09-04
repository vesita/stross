//! 节点图：节点注册与能力聚合。

use std::collections::HashMap;
use std::sync::Mutex;

use stross_proto::message::NodeId;

// 节点拓扑类型单一真源在 stross-node（v3.1 V1 去重，docs/framework-v3.md
// §10.4）：本模块不再重复定义 NodeInfo / NodeRole / TransportAddr，统一转发
// 概念 crate 类型；kernel 根部 `pub use graph::{NodeInfo, NodeRole, TransportAddr}`
// 继续重导出，路径不变。
pub use stross_node::{NodeInfo, NodeRole, TransportAddr};

/// 节点图：节点注册与能力聚合。
#[derive(Default)]
pub(crate) struct NodeGraph {
    pub(crate) nodes: Mutex<HashMap<NodeId, NodeInfo>>,
}
