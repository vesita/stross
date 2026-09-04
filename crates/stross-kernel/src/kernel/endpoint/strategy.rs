//! 策略解析（v3 §2.2 策略注册表模式）：`(节点, 端点, 策略) → 策略组合` 查表。
//!
//! 模块拆分（v3 §7）：从统一注册表拆出的策略域——[`UnifiedRegistry`] 的
//! `resolve_strategy`（策略解析）与 `stream_profile`（传输档案，语义流 id
//! 派生的同源三要素）。

use stross_proto::message::{EndpointId, EndpointStrategy, NodeId, PickRule, ReliabilityProfile};

use super::UnifiedRegistry;

impl UnifiedRegistry {
    /// 策略解析（统一查表）：`registry[节点][端点][策略]` → 策略组合。
    ///
    /// * 本机：端点声明策略单一真源（任何策略 id 都收敛到声明策略）；
    /// * 互联节点：从端点表条目取；`strategy_id` 缺省 = 端点默认策略（首个）。
    ///
    /// v3 存储解耦后按 [`EndpointId`] 直查端点表（不再走 node.endpoints.get）。
    pub fn resolve_strategy(
        &self,
        node_id: &NodeId,
        endpoint_id: EndpointId,
        strategy_id: Option<&str>,
    ) -> Option<EndpointStrategy> {
        let entry = self.endpoints.endpoint_entry(endpoint_id)?;
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
    /// 可推导同一流 id）。
    pub fn stream_profile(
        &self,
        node_id: &NodeId,
        endpoint_id: EndpointId,
    ) -> Option<(ReliabilityProfile, PickRule)> {
        let _ = node_id; // v3 平级端点表：按 EndpointId 直查（本机/远端同源）
        let entry = self.endpoints.endpoint_entry(endpoint_id)?;
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
