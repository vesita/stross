//! 统一注册表（v3 §4/§3.2）：**节点表（持端点引用）+ 独立端点表**。
//!
//! 模块拆分（v3 §7）：本文件承载 [`UnifiedRegistry`]（含节点 DTO 与查询投影）——
//! 策略解析（`resolve_strategy` / `stream_profile`）在 [`super::strategy`]、
//! 订阅端点生成工厂在 [`super::subscribe_generate`]。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use stross_endpoint::contract::{EndpointClass, ShareEndpoint, SubscribeCtx, TargetKind};
use stross_proto::message::{
    CodecId, Delivery, EndpointDir, EndpointId, EndpointManifest, EndpointState, EndpointStrategy,
    EndpointSummary, NodeId, PickRule, ReliabilityProfile, TransportPreference, Visibility,
};

use super::file_source::FileSource;
use super::subscribe_generate::SubscribeEndpointFactory;
use super::{EndpointEntry, EndpointRegistry};
use crate::Kernel;
use crate::error::Result;

/// 统一注册表（v3 §4）：**节点表（持端点引用）+ 独立端点表**。
///
/// * 节点表（[`NodeEntry`]）：身份 + `endpoint_ids` **引用**（本机与互联节点
///   都在同一张表，「本机」只是 `is_self` 标记，无 local/nodes 双路径）；
/// * 端点表（[`EndpointRegistry`]）：行为对象 + 策略 + 状态（`owner` 关联
///   归属节点）——「节点上的端点」= 按 `endpoint_ids` 查端点表的查询投影
///   （[`Self::node_endpoints`]），订阅统一按 `(节点 id, 端点 id, 策略 id)`
///   查表，自订与订其它互联节点走同一套逻辑。
pub struct UnifiedRegistry {
    /// 节点表：node_id → 身份 + 端点引用（本机亦入表，`is_self` 标记）。
    pub(super) nodes: HashMap<NodeId, NodeEntry>,
    /// 端点表：**独立存储**（行为对象单一真源；节点只持引用）。
    pub(super) endpoints: EndpointRegistry,
    /// 本机节点 id（身份注入；未注入时缺省 `NodeId::NIL`）。
    pub(super) self_node: NodeId,
    /// 本机节点展示名（身份注入）。
    pub(super) self_name: String,
    /// 订阅端点生成工厂表（v3 §2.2 策略注册表模式）：`EndpointClass` → 工厂。
    /// 默认注册 File → 文件订阅端、Graph/Audio → 媒体订阅端（构造处注册，
    /// 保持现状行为）；新增端点类注册工厂即扩展，不改分派逻辑。
    pub(super) subscribe_factories: HashMap<EndpointClass, SubscribeEndpointFactory>,
}

/// 一个节点（手机/电脑/本机）的注册：身份 + 它拥有的端点**引用**。
///
/// §3.1/§3.2 定稿：节点拥有端点（领域层级），但**持有的是引用**——
/// 端点行为对象在独立端点表（平级存储），「节点上的端点」= 查询投影，
/// 不把端点注册信息嵌进节点结构（v2 嵌套是耦合根源）。
#[derive(Debug, Clone)]
pub struct NodeEntry {
    /// 节点 id（mDNS/目录权威；本机 = 注入身份）。
    pub node_id: NodeId,
    /// 展示名（device_name / 本机名）。
    pub name: String,
    /// 协商/目录入口（`host:port`；本机为 `"local"`）。
    pub addr: String,
    /// 是否本机（普通节点标记，非特殊身份）。
    pub is_self: bool,
    /// 端点**引用**（「节点上的端点」= 按本表投影端点表，非嵌套存储）。
    pub endpoint_ids: Vec<EndpointId>,
}

/// 一个互联节点的注册 DTO（**纯展示快照**，从端点表投影构造，不做存储；
/// 存储层只有 [`NodeEntry::endpoint_ids`] 引用）。
#[derive(Debug, Clone)]
pub struct NodeRegistration {
    /// 互联节点 id（mDNS/目录权威）。
    pub node_id: NodeId,
    /// 展示名（device_name）。
    pub name: String,
    /// 协商/目录入口（`host:port`；本机为 `"local"`）。
    pub addr: String,
    /// 是否本机（便于 UI 高亮；非特殊身份）。
    pub is_self: bool,
    /// 该互联节点的下属端点（**投影**：按 endpoint_ids 查端点表构造）。
    pub endpoints: HashMap<EndpointId, EndpointRegistration>,
}

/// 一个端点（节点上可分享的具体内容，如"屏幕"/"麦克风"）的注册 DTO。
#[derive(Debug, Clone)]
pub struct EndpointRegistration {
    pub endpoint_id: EndpointId,
    pub kind: stross_proto::message::MediaKind,
    pub name: String,
    /// 目标类型（由协商档案推断；远端不落 wire）。
    pub target: TargetKind,
    /// 传输可靠性档案（目录清单携带；语义流 id 派生与订阅规格组装的同源要素）。
    pub transport_profile: ReliabilityProfile,
    /// 端点自主声明的策略组合（策略独立可寻址，同一内容可有多种处理组合）。
    /// **保持清单声明顺序**：默认策略 = 首个（与 [`EndpointManifest::strategy`]
    /// 的确定性语义一致——曾用 HashMap 迭代序非确定性选取默认策略）。
    pub strategies: Vec<EndpointStrategy>,
}

impl Default for UnifiedRegistry {
    fn default() -> Self {
        // 本机自节点条目恒在节点表（未注入身份时缺省 NIL——快照恒含本机，
        // 与 v2 行为一致）。
        let mut nodes = HashMap::new();
        nodes.insert(
            NodeId::NIL,
            NodeEntry {
                node_id: NodeId::NIL,
                name: "本机".into(),
                addr: "local".into(),
                is_self: true,
                endpoint_ids: vec![],
            },
        );
        Self {
            nodes,
            endpoints: EndpointRegistry::new(),
            self_node: NodeId::NIL,
            self_name: "本机".into(),
            subscribe_factories: super::subscribe_generate::default_factories(),
        }
    }
}

impl UnifiedRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注入本机节点身份（`is_self` 判定与 `(节点, 端点, 策略)` 查表的
    /// 本机分支用；身份未注入时本机缺省 id 为 `NIL`）。
    ///
    /// 幂等迁移：旧自节点条目（含 `endpoint_ids` 引用）与端点表 `owner` 同步
    /// 迁移到新 id（覆盖「先 seed 后注入身份」的装配顺序）。
    pub fn set_self_node(&mut self, node_id: impl Into<NodeId>, name: &str) {
        let new_id = node_id.into();
        let old_id = self.self_node;
        if old_id != new_id {
            if let Some(mut entry) = self.nodes.remove(&old_id) {
                entry.node_id = new_id;
                entry.name = name.to_string();
                entry.is_self = true;
                self.nodes.insert(new_id, entry);
            } else {
                self.nodes.insert(
                    new_id,
                    NodeEntry {
                        node_id: new_id,
                        name: name.to_string(),
                        addr: "local".into(),
                        is_self: true,
                        endpoint_ids: vec![],
                    },
                );
            }
            for e in self.endpoints.endpoints.values_mut() {
                if e.owner == old_id {
                    e.owner = new_id;
                }
            }
        } else if let Some(entry) = self.nodes.get_mut(&new_id) {
            entry.name = name.to_string();
            entry.is_self = true;
        } else {
            self.nodes.insert(
                new_id,
                NodeEntry {
                    node_id: new_id,
                    name: name.to_string(),
                    addr: "local".into(),
                    is_self: true,
                    endpoint_ids: vec![],
                },
            );
        }
        self.self_node = new_id;
        self.self_name = name.to_string();
    }

    /// 本机节点 id（订阅查表的本机分支键）。
    pub fn self_node_id(&self) -> NodeId {
        self.self_node
    }

    /// 确保本机自节点条目存在（seed / publish_file 追加端点引用前调用）。
    fn ensure_self_entry(&mut self) {
        if !self.nodes.contains_key(&self.self_node) {
            self.nodes.insert(
                self.self_node,
                NodeEntry {
                    node_id: self.self_node,
                    name: self.self_name.clone(),
                    addr: "local".into(),
                    is_self: true,
                    endpoint_ids: vec![],
                },
            );
        }
    }

    // -- 端点表（本机行为对象 + 远端登记；行为对象只在本表） --

    /// 登记本机端点：插入端点表（owner = 本机节点）+ 把 id 追加到本机
    /// 节点条目（幂等：已登记不重复；引用去重）。
    pub fn seed(&mut self, ep: Box<dyn ShareEndpoint>) -> bool {
        let id = ep.id();
        if !self.endpoints.seed_with_owner(ep, self.self_node) {
            return false;
        }
        self.ensure_self_entry();
        let list = &mut self
            .nodes
            .get_mut(&self.self_node)
            .expect("自节点条目已建")
            .endpoint_ids;
        if !list.contains(&id) {
            list.push(id);
        }
        true
    }

    pub fn endpoint_arc(&self, endpoint_id: EndpointId) -> Option<Arc<dyn ShareEndpoint>> {
        self.endpoints.endpoint_arc(endpoint_id)
    }

    pub fn target(&self, endpoint_id: EndpointId) -> Option<TargetKind> {
        self.endpoints.target(endpoint_id)
    }

    /// 本机端点清单（本机目录用；含未通告；owner 过滤 = 本机）。
    pub fn manifests(&self) -> Vec<EndpointManifest> {
        self.endpoints.manifests_of(Some(self.self_node))
    }

    /// 本机已通告端点清单（对端目录用；Private 过滤由调用方做）。
    pub fn published_manifests(&self) -> Vec<EndpointManifest> {
        self.endpoints.published_manifests_of(Some(self.self_node))
    }

    /// 本机 mDNS 摘要（L1）：全部端点（含不可挂载 + 未通告标记）。
    pub fn summaries(&self) -> Vec<EndpointSummary> {
        self.endpoints.summaries_of(Some(self.self_node))
    }

    /// 端点清单（订阅握手 / 目录 API 用；按 id 直查，任意归属）。
    pub fn manifest(&self, endpoint_id: EndpointId) -> Option<EndpointManifest> {
        self.endpoints.manifest(endpoint_id)
    }

    pub fn publish(
        &mut self,
        endpoint_id: EndpointId,
        visibility: Visibility,
        delivery: Delivery,
        transports: Vec<TransportPreference>,
        codecs: Vec<CodecId>,
    ) -> Result<EndpointManifest> {
        self.endpoints
            .publish(endpoint_id, visibility, delivery, transports, codecs)
    }

    pub fn unpublish(&mut self, endpoint_id: EndpointId) -> Result<()> {
        self.endpoints.unpublish(endpoint_id)
    }

    /// 公开本机文件端点（动态端点；插入端点表 owner = 本机 + 追加本机引用）。
    pub fn publish_file(
        &mut self,
        path: &Path,
        visibility: Visibility,
        delivery: Delivery,
    ) -> Result<EndpointManifest> {
        let m = self
            .endpoints
            .publish_file(path, visibility, delivery, self.self_node)?;
        self.ensure_self_entry();
        let id = EndpointId::new(m.kind, m.endpoint_id);
        let list = &mut self
            .nodes
            .get_mut(&self.self_node)
            .expect("自节点条目已建")
            .endpoint_ids;
        if !list.contains(&id) {
            list.push(id);
        }
        Ok(m)
    }

    pub fn file_source(&self, endpoint_id: EndpointId) -> Option<&FileSource> {
        self.endpoints.file_source(endpoint_id)
    }

    pub fn set_state(
        &mut self,
        endpoint_id: EndpointId,
        state: EndpointState,
        subscribers: u32,
    ) -> bool {
        self.endpoints.set_state(endpoint_id, state, subscribers)
    }

    pub fn on_subscribed(
        &self,
        app: &Arc<Kernel>,
        endpoint_id: EndpointId,
        ctx: &SubscribeCtx,
    ) -> bool {
        self.endpoints.on_subscribed(app, endpoint_id, ctx)
    }

    pub fn note_subscriber(&mut self, endpoint_id: EndpointId, node_id: NodeId) {
        self.endpoints.note_subscriber(endpoint_id, node_id);
    }

    pub fn note_unsubscriber(&mut self, endpoint_id: EndpointId, node_id: NodeId) -> u32 {
        self.endpoints.note_unsubscriber(endpoint_id, node_id)
    }

    pub fn clear_subscribers(&mut self, endpoint_id: EndpointId) {
        self.endpoints.clear_subscribers(endpoint_id);
    }

    pub fn default_transports(target: TargetKind) -> Vec<TransportPreference> {
        EndpointRegistry::default_transports(target)
    }

    // -- 节点表：目录映射 + 查询投影 --

    /// 把目录响应（`GET /api/endpoints`）的互联节点映射进注册表：
    /// 登记/更新 [`NodeEntry`]（含端点**引用**）+ 把远端端点插入端点表
    /// （owner = 该节点；[`EndpointRegistry::register_remote`]——同
    /// `EndpointId` 更新不重复）。幂等：同节点重复拉取覆盖（目录是权威快照）。
    pub fn register_remote_directory(&mut self, dir: &EndpointDir, addr: &str) {
        let node_id = dir.node.node_id;
        if node_id.is_empty() || node_id == self.self_node {
            return; // 空节点 / 本机镜像不入远端登记（本机走自节点条目）
        }
        let ids: Vec<EndpointId> = dir
            .endpoints
            .iter()
            .map(|m| EndpointId::new(m.kind, m.endpoint_id))
            .collect();
        for m in &dir.endpoints {
            self.endpoints.register_remote(m, node_id);
        }
        self.nodes.insert(
            node_id,
            NodeEntry {
                node_id,
                name: dir.node.node_name.clone(),
                addr: addr.to_string(),
                is_self: false,
                endpoint_ids: ids,
            },
        );
    }

    /// 查询投影：按节点 `endpoint_ids` 查端点表（「节点上的端点」= 投影，
    /// 非嵌套存储）。
    pub fn node_endpoints(&self, node_id: &NodeId) -> impl Iterator<Item = &EndpointEntry> + '_ {
        self.nodes.get(node_id).into_iter().flat_map(|n| {
            n.endpoint_ids
                .iter()
                .filter_map(|id| self.endpoints.endpoint_entry(*id))
        })
    }

    /// 注册表快照（含本机；UI / 调试用）：节点 → 端点 → 策略——**纯展示 DTO
    /// 投影**（[`NodeRegistration`]，从节点表引用 + 端点表构造，不做存储）。
    pub fn node_registrations(&self) -> Vec<NodeRegistration> {
        let mut v: Vec<NodeRegistration> = self
            .nodes
            .values()
            .map(|n| NodeRegistration {
                node_id: n.node_id,
                name: n.name.clone(),
                addr: n.addr.clone(),
                is_self: n.is_self,
                endpoints: n
                    .endpoint_ids
                    .iter()
                    .filter_map(|id| {
                        let entry = self.endpoints.endpoint_entry(*id)?;
                        Some((*id, registration_of(entry)))
                    })
                    .collect(),
            })
            .collect();
        v.sort_by_key(|a| a.node_id);
        v
    }

    /// 端点显式订阅节点集（[`ShareService::active`] 的 `subscriber_nodes` 投影
    /// 真源；`note_subscriber` / `note_unsubscriber` 维护）。
    pub fn subscriber_nodes(&self, endpoint_id: EndpointId) -> Option<Vec<NodeId>> {
        self.endpoints.subscriber_nodes(endpoint_id)
    }

    /// 全部节点 id（订阅查表键；含本机）。
    pub fn node_ids(&self) -> Vec<NodeId> {
        let mut ids: Vec<NodeId> = self.nodes.keys().copied().collect();
        ids.sort();
        ids
    }
}

/// 端点条目 → 展示 DTO（[`NodeRegistration`] 投影用）：策略组合保持条目声明
/// 顺序（默认 = 首个）。
fn registration_of(entry: &EndpointEntry) -> EndpointRegistration {
    let m = EndpointRegistry::manifest_of(entry);
    let id = entry.ep.id();
    EndpointRegistration {
        endpoint_id: id,
        kind: id.kind,
        name: entry.ep.name().to_string(),
        target: target_from_manifest(&m),
        transport_profile: m.transport_profile,
        strategies: entry.strategies.clone(),
    }
}

/// 从清单推导目标类型（远端目标类型不落 wire；按协商档案推断——
/// Lossless + StrictOrdered 为确定目标，其余为实时目标）。
pub(super) fn target_from_manifest(m: &EndpointManifest) -> TargetKind {
    if m.transport_profile == ReliabilityProfile::Lossless && m.pick_rule == PickRule::StrictOrdered
    {
        TargetKind::Determined
    } else {
        TargetKind::Live
    }
}

/// 清单 → 策略组合表（缺省由平铺 `pick_rule` 推导直通 + pick 的默认策略；
/// **保持清单声明顺序**——确定性默认 = 首个，与 [`EndpointManifest::strategy`]
/// 单一真源一致）。
pub(super) fn strategies_of(m: &EndpointManifest) -> Vec<EndpointStrategy> {
    if !m.strategies.is_empty() {
        return m.strategies.clone();
    }
    vec![m.strategy(None)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use stross_endpoint::contract::{Probe, ShareEndpoint};
    use stross_endpoint::share::file::FileEndpoint;
    use stross_proto::message::{
        CodecId, Delivery, EndpointDir, EndpointId, EndpointManifest, EndpointNode, EndpointState,
        EndpointStrategy, MediaKind, NodeId, StrategyId, SubscribeSpec, Visibility,
    };
    use stross_proto::time::unix_secs;

    fn ok_probe() -> Probe {
        Arc::new(|| Ok(()))
    }

    fn screen() -> Box<dyn ShareEndpoint> {
        Box::new(stross_endpoint::share::screen::ScreenEndpoint::new(
            "屏幕",
            ok_probe(),
        ))
    }

    fn remote_dir(node_id: &str, name: &str) -> EndpointDir {
        EndpointDir {
            node: EndpointNode {
                node_id: node_id.into(),
                node_name: name.into(),
            },
            endpoints: vec![
                EndpointManifest {
                    endpoint_id: 0,
                    kind: MediaKind::Screen,
                    name: "屏幕".into(),
                    available: true,
                    last_error: None,
                    published: true,
                    visibility: Visibility::Public,
                    delivery: Delivery::Pull,
                    transports: EndpointRegistry::default_transports(TargetKind::Live),
                    transport_profile: stross_proto::message::ReliabilityProfile::Lossy,
                    pick_rule: stross_proto::message::PickRule::Realtime,
                    strategies: vec![EndpointStrategy {
                        strategy_id: EndpointStrategy::DEFAULT_ID,
                        serialize: stross_proto::message::SerializeRule::Passthrough,
                        pick: stross_proto::message::PickRule::Realtime,
                    }],
                    codecs: vec![CodecId::H264],
                    state: EndpointState::Idle,
                    subscribers: 0,
                    updated_at: unix_secs(),
                },
                EndpointManifest {
                    endpoint_id: 0,
                    kind: MediaKind::File,
                    name: "notes.txt".into(),
                    available: true,
                    last_error: None,
                    published: true,
                    visibility: Visibility::Public,
                    delivery: Delivery::Pull,
                    transports: EndpointRegistry::default_transports(TargetKind::Determined),
                    transport_profile: stross_proto::message::ReliabilityProfile::Lossless,
                    pick_rule: stross_proto::message::PickRule::StrictOrdered,
                    strategies: vec![EndpointStrategy {
                        strategy_id: EndpointStrategy::DEFAULT_ID,
                        serialize: stross_proto::message::SerializeRule::Passthrough,
                        pick: stross_proto::message::PickRule::StrictOrdered,
                    }],
                    codecs: vec![],
                    state: EndpointState::Idle,
                    subscribers: 0,
                    updated_at: unix_secs(),
                },
            ],
        }
    }

    /// 统一注册表：本机与互联节点同一张端点表（节点持引用），订阅按
    /// (节点, 端点, 策略) 统一查表。
    #[test]
    fn unified_registry_three_layer_lookup() {
        let mut reg = UnifiedRegistry::new();
        reg.set_self_node("node-pc", "电脑");
        assert!(reg.seed(screen()), "本机端点登记（owner = 自节点）");
        assert!(reg.seed(Box::new(FileEndpoint::new(
            EndpointId::new(MediaKind::File, 0),
            "本地.txt".into(),
            std::env::temp_dir().join("stross-unified.txt"),
        ))));

        // 本机查表：registry[本机][端点][策略] → 策略组合（strategy() 单一真源）
        let s = reg
            .resolve_strategy(
                &NodeId::from("node-pc"),
                EndpointId::new(MediaKind::Screen, 0),
                None,
            )
            .expect("本机屏幕端点应可解析");
        assert_eq!(s.strategy_id, StrategyId::Default);
        assert_eq!(s.pick, stross_proto::message::PickRule::Realtime);
        assert_eq!(
            s.serialize,
            stross_proto::message::SerializeRule::Passthrough
        );
        // 本机文件端点：严格顺序 + Lossless 推断为确定目标
        let fs = reg
            .resolve_strategy(
                &NodeId::from("node-pc"),
                EndpointId::new(MediaKind::File, 0),
                Some("default"),
            )
            .expect("本机文件端点应可解析");
        assert_eq!(fs.pick, stross_proto::message::PickRule::StrictOrdered);
        // 未知端点 → None
        assert!(
            reg.resolve_strategy(
                &NodeId::from("node-pc"),
                EndpointId::new(MediaKind::Service, 99),
                None
            )
            .is_none()
        );

        // 互联节点映射（目录拉取 → 节点条目 + 端点表远端登记）
        reg.register_remote_directory(&remote_dir("node-phone", "手机A"), "192.168.1.5:18779");
        let s = reg
            .resolve_strategy(
                &NodeId::from("node-phone"),
                EndpointId::new(MediaKind::Screen, 0),
                None,
            )
            .expect("远端屏幕端点应可解析");
        assert_eq!(s.pick, stross_proto::message::PickRule::Realtime);
        let f = reg
            .resolve_strategy(
                &NodeId::from("node-phone"),
                EndpointId::new(MediaKind::File, 0),
                Some("default"),
            )
            .expect("远端文件端点应可解析");
        assert_eq!(f.pick, stross_proto::message::PickRule::StrictOrdered);
        // 未知策略 id → None（策略独立可寻址）
        assert!(
            reg.resolve_strategy(
                &NodeId::from("node-phone"),
                EndpointId::new(MediaKind::Screen, 0),
                Some("nope")
            )
            .is_none()
        );

        // 查询投影：按节点 endpoint_ids 查端点表（行为对象只存在于端点表）
        let self_ids: Vec<EndpointId> = reg
            .node_endpoints(&NodeId::from("node-pc"))
            .map(|e| e.ep.id())
            .collect();
        assert_eq!(self_ids.len(), 2, "本机两个端点引用");
        assert_eq!(
            reg.node_endpoints(&NodeId::from("node-phone")).count(),
            2,
            "手机两个端点引用"
        );
        assert_eq!(
            reg.node_endpoints(&NodeId::from("node-phone"))
                .next()
                .map(|e| e.owner),
            Some(NodeId::from("node-phone")),
            "远端端点 owner 关联归属节点"
        );

        // 快照：本机 + 互联节点都在同一张表（含 is_self 标记）
        let nodes = reg.node_registrations();
        assert_eq!(nodes.len(), 2, "本机 + 手机两台节点");
        let self_node = nodes.iter().find(|n| n.is_self).expect("本机镜像在表内");
        assert_eq!(self_node.node_id, NodeId::from("node-pc"));
        assert!(
            self_node
                .endpoints
                .contains_key(&EndpointId::new(MediaKind::Screen, 0))
        );
        let phone = nodes
            .iter()
            .find(|n| n.node_id == NodeId::from("node-phone"))
            .expect("手机节点在表内");
        assert!(!phone.is_self);
        assert_eq!(phone.endpoints.len(), 2);

        // 订阅端点生成（按能力族分发）：File → 文件订阅端（接收落盘）；
        // Graph/Audio → 媒体订阅端（播放器入端点）
        let spec = SubscribeSpec {
            node_id: "node-phone".into(),
            kind: MediaKind::File,
            endpoint_id: 0,
            strategy_id: Some("default".into()),
            strategy: f,
            delivery: Delivery::Pull,
            stream_id: "sess-1".into(),
            relay_url: Some("ws://192.168.1.5:18777".into()),
        };
        let ep = reg.generate_subscribe_endpoint(&spec, Some(Path::new("/tmp/stross-recv")));
        let ep = ep.expect("文件订阅端应可生成");
        assert_eq!(ep.kind(), MediaKind::File);
        assert_eq!(
            crate::EndpointClass::from_kind(ep.kind()),
            crate::EndpointClass::File,
            "文件能力族"
        );
        let spec_media = SubscribeSpec {
            node_id: "node-phone".into(),
            kind: MediaKind::Screen,
            endpoint_id: 0,
            strategy_id: None,
            strategy: s,
            delivery: Delivery::Pull,
            stream_id: "sess-2".into(),
            relay_url: Some("ws://192.168.1.5:18777".into()),
        };
        let media_ep = reg
            .generate_subscribe_endpoint(&spec_media, None)
            .expect("Graph 类媒体订阅端点（播放器入端点）应可生成");
        assert_eq!(media_ep.kind(), MediaKind::Screen);
        assert_eq!(
            crate::EndpointClass::from_kind(media_ep.kind()),
            crate::EndpointClass::Graph,
            "屏幕归 Graph 能力族"
        );
    }

    /// 旧目录 wire（无 strategies 字段）映射：按平铺 pick_rule 推导默认策略，
    /// 统一查表仍可用（旧对端兼容）。
    #[test]
    fn unified_registry_remote_without_strategies_derives_default() {
        let mut reg = UnifiedRegistry::new();
        reg.set_self_node("node-pc", "电脑");
        let mut dir = remote_dir("node-old", "旧对端");
        dir.endpoints[0].strategies = vec![]; // 旧 wire：无策略列表
        dir.endpoints[0].pick_rule = stross_proto::message::PickRule::Realtime;
        reg.register_remote_directory(&dir, "192.168.1.9:18779");
        let s = reg
            .resolve_strategy(
                &NodeId::from("node-old"),
                EndpointId::new(MediaKind::Screen, 0),
                None,
            )
            .expect("旧对端策略应推导成功");
        assert_eq!(s.strategy_id, StrategyId::Default);
        assert_eq!(s.pick, stross_proto::message::PickRule::Realtime);
        assert_eq!(
            s.serialize,
            stross_proto::message::SerializeRule::Passthrough
        );
    }
}
