//! 策略解析（v3 §2.2 策略注册表模式）：`(节点, 端点, 策略) → 策略组合` 查表。
//!
//! 模块拆分（v3 §7）：从统一注册表拆出的策略域——[`UnifiedRegistry`] 的
//! `resolve_strategy`（策略解析）与 `stream_profile`（传输档案，语义流 id
//! 派生的同源三要素）。
//!
//! v3.1 §10.5（插件挂载表复合键）：查表一律按 `(宿主, 端点)` 复合键
//! [`EndpointRef`] 精确取——**不再忽略 node_id**（旧实现 `let _ = node_id;`
//! 扁平查表导致跨节点同 id 遮蔽：远端 `screen:0` 会覆盖本机条目）。

use stross_proto::message::{
    EndpointId, EndpointRef, EndpointStrategy, NodeId, PickRule, ReliabilityProfile,
};

use super::UnifiedRegistry;

/// 节点规范化：本机兼容键（身份未注入时缺省 NIL / "local" 旧路径）统一到
/// `self_node`——插件挂载表键恒用**实际宿主节点 id**（本机条目注册时
/// owner = self_node）。
pub(super) fn normalize_node(node: &NodeId, self_node: NodeId) -> NodeId {
    if is_self_node(*node, self_node) {
        self_node
    } else {
        *node
    }
}

impl UnifiedRegistry {
    /// 策略解析（统一查表）：`registry[节点][端点][策略]` → 策略组合。
    ///
    /// * 本机：端点声明策略单一真源（任何策略 id 都收敛到声明策略）；
    /// * 互联节点：从插件挂载表按 `(宿主, 端点)` 复合键取条目；
    ///   `strategy_id` 缺省 = 端点默认策略（首个）。
    pub fn resolve_strategy(
        &self,
        node_id: &NodeId,
        endpoint_id: EndpointId,
        strategy_id: Option<&str>,
    ) -> Option<EndpointStrategy> {
        let key = EndpointRef::new(normalize_node(node_id, self.self_node), endpoint_id);
        let entry = self.endpoints.endpoint_entry(key)?;
        if is_self_node(*node_id, self.self_node) {
            // 本机单策略：任何 id 都收敛到端点声明的策略
            return entry.strategies.first().cloned();
        }
        match strategy_id {
            Some(id) => entry
                .strategies
                .iter()
                .find(|s| s.strategy_id.as_str() == id)
                .cloned(),
            None => entry.strategies.first().cloned(),
        }
    }

    /// 端点传输档案（本机/互联节点统一）：`(transport_profile, 默认策略 pick)`
    /// ——语义流 id 派生的**同源三要素**（与共享方协商签发 `compose_grant` 的
    /// `derive(endpoint_id, m.transport_profile, m.pick_rule)` 一致，订阅方本地
    /// 可推导同一流 id）。v3.1 §10.5：按 `(宿主, 端点)` 复合键取，节点限定。
    pub fn stream_profile(
        &self,
        node_id: &NodeId,
        endpoint_id: EndpointId,
    ) -> Option<(ReliabilityProfile, PickRule)> {
        let key = EndpointRef::new(normalize_node(node_id, self.self_node), endpoint_id);
        let entry = self.endpoints.endpoint_entry(key)?;
        let pick = entry
            .strategies
            .first()
            .map(|s| s.pick)
            .unwrap_or(PickRule::Realtime);
        Some((entry.ep.transport_profile(), pick))
    }
}

/// 本机节点判定（`(节点, 端点, 策略)` 查表的本机分支；身份未注入时缺省
/// NIL / "local" 兼容旧路径）。
fn is_self_node(node_id: NodeId, self_node: NodeId) -> bool {
    node_id == self_node || node_id == NodeId::NIL || node_id == NodeId::from_seed("local")
}
