//! 端点注册表：**插件挂载表**（v3.1 §10.5：层级进地址）+ 通告参数管理。
//!
//! 设计规格：docs/framework-v3.md §3.2（端点表）/ §4（内核结构）/ §7（模块拆分）
//! / §10.5（插件挂载表复合键）。
//!
//! 模块拆分（v3 §7）：`kernel/endpoint/` 目录——
//! * [`self`]（本文件）：插件挂载表核心（[`EndpointEntry`] / [`EndpointRegistry`] /
//!   远端存根 [`RemoteEndpoint`]）+ 通告参数管理；
//! * [`registry`]：统一注册表 [`UnifiedRegistry`]（节点表持端点引用 + 查询投影）
//!   与节点 DTO（[`NodeEntry`] / [`NodeRegistration`] / [`EndpointRegistration`]）；
//! * [`strategy`]：策略解析（`resolve_strategy` / `stream_profile`）；
//! * [`subscribe_generate`]：订阅端点生成工厂表（[`SubscribeEndpointFactory`] +
//!   `generate_subscribe_endpoint`）；
//! * [`file_source`]：[`FileSource`]（文件端点本地文件源）。
//!
//! * 端点 = 节点上可共享的能力实体（插件）；**契约（[`Endpoint`] / [`SubscribeCtx`] /
//!   [`Probe`] 等）与具体端点实现（屏幕 / 麦克风 / 系统声音 / 文件）在
//!   [`stross_endpoint`] 插件区**，本模块只做身份登记、通告参数管理与订阅联动
//!   （内核 = 纯管理调度，不做媒体数据面）；
//! * 端点自维护「可挂载性」（`available`，load 探测结果）与失败原因
//!   （`last_error`）；注册表只做身份登记与通告参数管理；
//! * **存储解耦（v3 §3.2）**：节点表（[`NodeEntry`]，含 `endpoint_ids` **引用**）
//!   与独立端点表（[`EndpointRegistry`]）——端点行为对象**只存在于端点表**，
//!   「节点上的端点」是查询投影（[`UnifiedRegistry::node_endpoints`]），**不是
//!   把端点注册信息嵌进节点结构**（v2 `NodeRegistration { endpoints: HashMap<…> }`
//!   嵌套是耦合根源）。本机与互联节点（目录/发现映射）在同一张端点表——订阅
//!   统一按 `(节点 id, 端点 id, 策略 id) → 策略组合` 查表，自订与订其它互联节点
//!   走同一套逻辑；策略 = 序列化规则 + pick 规则（[`EndpointStrategy`]），
//!   传输档案不进注册表；
//! * **插件挂载表（v3.1 §10.5）**：表键 = [`EndpointRef`]（宿主节点 + 端点句柄
//!   复合键，**层级进地址**）——节点拥有端点（领域层级）无法展平，同一
//!   `screen:0` 可在不同宿主节点各自挂载；**读路径一律节点限定**，按
//!   (宿主, 端点) 精确取，杜绝跨节点同 id 遮蔽；
//! * **远端端点 = 存根登记**（[`RemoteEndpoint`]）：目录只携带展示元数据
//!   （无本机行为对象——远端端点不可本机 `share`）；按 `(宿主, 端点)` 分键，
//!   同节点重复拉取原位更新（幂等，目录是权威快照），不同节点同 id 端点共存
//!   （无「后登记覆盖」遮蔽，见 [`EndpointRegistry::register_remote`]）；
//! * **订阅联动**：`on_subscribed` 出锁克隆端点对象后调用其 `share`
//!   （端点自驱动，内核不做类型分派）；订阅端由
//!   [`UnifiedRegistry::generate_subscribe_endpoint`] 生成（订阅端点生成）。

mod file_source;
mod registry;
mod strategy;
mod subscribe_generate;

pub use file_source::FileSource;
pub use registry::{EndpointRegistration, NodeEntry, NodeRegistration, UnifiedRegistry};
pub use subscribe_generate::SubscribeEndpointFactory;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use stross_endpoint::contract::{
    EndpointBase, Runtime, ShareEndpoint, ShareHost, SubscribeCtx, TargetKind,
};
use stross_endpoint::share::file::FileEndpoint;
use stross_proto::message::{
    CodecId, Delivery, EndpointId, EndpointManifest, EndpointRef, EndpointState, EndpointStrategy,
    EndpointSummary, MediaKind, NodeId, ReliabilityProfile, TransportId, TransportPreference,
    Visibility,
};
use stross_proto::time::unix_secs;

use self::registry::{strategies_of, target_from_manifest};
use crate::Kernel;
use crate::error::{Error, Result};

/// 能力族展示顺序（与 [`stross_endpoint::factory::platform_endpoints`] 的种子顺序
/// 一致：屏幕 → 音频 → 文件…）。未知 kind 排最后。
fn kind_rank(kind: MediaKind) -> u8 {
    match kind {
        MediaKind::Screen => 0,
        MediaKind::Window => 1,
        MediaKind::Camera => 2,
        MediaKind::Mic => 3,
        MediaKind::SystemAudio => 4,
        MediaKind::File => 5,
        MediaKind::Clipboard => 6,
        MediaKind::Input => 7,
        MediaKind::Service => 8,
    }
}

/// 端点清单排序：能力族固定顺序（主）+ 端点 id（稳定次键，避免同名同族乱序）。
fn endpoint_order(a: &EndpointManifest, b: &EndpointManifest) -> std::cmp::Ordering {
    kind_rank(a.kind)
        .cmp(&kind_rank(b.kind))
        .then_with(|| a.endpoint_id.cmp(&b.endpoint_id))
}

/// 端点条目：行为对象（[`Endpoint`]）+ 通告参数（公开者声明）。
///
/// §3.2 定稿：**行为对象只存在本表**（节点表只持 `endpoint_ids` 引用）；
/// 归属节点**已编码进表键**（[`EndpointRef`]，v3.1 §10.5——条目自身不再持
/// `owner` 字段，杜绝「条目与键不一致」的漂移面）。
pub struct EndpointEntry {
    pub ep: Arc<dyn ShareEndpoint>,
    /// 端点自主声明的策略组合（策略独立可寻址，同一内容可有多种处理组合）。
    /// 本机 = `ep.strategy()` 单一真源（seed 时快照）；远端 = 目录携带全量
    /// （**保持清单声明顺序**：默认策略 = 首个）。
    pub strategies: Vec<EndpointStrategy>,
    pub published: bool,
    pub visibility: Visibility,
    pub delivery: Delivery,
    pub transports: Vec<TransportPreference>,
    pub codecs: Vec<CodecId>,
    pub state: EndpointState,
    pub subscribers: u32,
    /// **显式订阅节点集**（订阅终止通知用）：订阅达成时记入、显式取消订阅时
    /// 移除——让共享端在「最后一个订阅者离开」的瞬间更新端点状态，不必等
    /// 数据面 watchers 断连检测（后者对强杀/断网场景有延迟）。渲染的
    /// `subscribers` 计数与该集大小一致（`set_state` 同源更新）。
    pub subscriber_nodes: HashSet<NodeId>,
    pub updated_at: u64,
}

/// 端点注册表：**插件挂载表**——行为对象（本机）+ 远端存根登记，键 =
/// [`EndpointRef`]（宿主节点 + 端点句柄**复合键**，v3.1 §10.5：层级进地址）。
///
/// v3 §3.2：平级存储，不嵌套在节点下；「节点上的端点」= 按节点
/// `endpoint_ids` 查本表（投影），见 [`super::UnifiedRegistry::node_endpoints`]。
/// v3.1 §10.5：同一 [`EndpointId`] 可在不同宿主节点各自挂载——读路径一律
/// 节点限定（按复合键精确取），杜绝跨节点同 id 遮蔽。
#[derive(Default)]
pub struct EndpointRegistry {
    /// 端点表（键 = [`EndpointRef`] 复合键：宿主节点 + 节点内端点句柄）：
    /// 本机行为对象 + 远端存根登记。
    endpoints: HashMap<EndpointRef, EndpointEntry>,
    /// 文件端点：复合键 → 本地文件源（control.rs 状态展示）。
    file_sources: HashMap<EndpointRef, FileSource>,
    /// 文件端点数值子 id 分配器（`file:<n>`；重名不再进 id，只影响展示名）。
    next_file_id: u32,
}

impl EndpointRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记本机端点（归属节点缺省 [`NodeId::NIL`]；见
    /// [`Self::seed_with_owner`]——统一注册表经它传本机节点 id）。
    pub fn seed(&mut self, ep: Box<dyn ShareEndpoint>) -> bool {
        self.seed_with_owner(ep, NodeId::NIL)
    }

    /// 登记端点并立即 `load`（探测可挂载性）；(宿主, 端点) 键已存在时返回
    /// `false`。
    ///
    /// load 失败不阻止登记：端点保留在表里但标记不可挂载（`available=false`
    /// + `last_error`）——UI 可见原因，不可通告/订阅。
    pub fn seed_with_owner(&mut self, mut ep: Box<dyn ShareEndpoint>, owner: NodeId) -> bool {
        let id = ep.id();
        let key = EndpointRef::new(owner, id);
        if self.endpoints.contains_key(&key) {
            return false;
        }
        if let Err(e) = ep.load() {
            tracing::warn!("端点 {id} load 失败，标记不可挂载: {e}");
        }
        let strategy = ep.strategy();
        self.endpoints.insert(
            key,
            EndpointEntry {
                ep: Arc::from(ep),
                strategies: vec![strategy],
                published: false,
                visibility: Visibility::Public,
                delivery: Delivery::Pull,
                transports: vec![],
                codecs: vec![],
                state: EndpointState::Idle,
                subscribers: 0,
                subscriber_nodes: HashSet::new(),
                updated_at: unix_secs(),
            },
        );
        true
    }

    /// 远端端点登记（目录拉取）：把清单映射为**存根条目**（[`RemoteEndpoint`]，
    /// 只承载展示元数据与策略组合）插入端点表（`owner` = 该节点）。
    ///
    /// **复合键语义（v3.1 §10.5）**：按 `(宿主, 端点)` 分键——同节点重复拉取
    /// 目录**原位更新**（幂等，目录是权威快照）；**不同节点的同 id 端点共存**，
    /// 不再有「后登记覆盖」遮蔽（`EndpointId` 只是节点内局部句柄，跨设备
    /// 唯一性靠 `(宿主, 端点)` 命名空间）。
    pub fn register_remote(&mut self, m: &EndpointManifest, owner: NodeId) {
        let key = EndpointRef::new(owner, EndpointId::new(m.kind, m.endpoint_id));
        let strategies = strategies_of(m);
        let strategy = strategies
            .first()
            .cloned()
            .unwrap_or_else(|| m.strategy(None));
        let ep: Box<dyn ShareEndpoint> = Box::new(RemoteEndpoint {
            base: EndpointBase {
                id: EndpointId::new(m.kind, m.endpoint_id),
                kind: m.kind,
                name: m.name.clone(),
                available: m.available,
                last_error: m.last_error.clone(),
            },
            target: target_from_manifest(m),
            transport_profile: m.transport_profile,
            strategy,
        });
        self.endpoints.insert(
            key,
            EndpointEntry {
                ep: Arc::from(ep),
                strategies,
                published: m.published,
                visibility: m.visibility.clone(),
                delivery: m.delivery,
                transports: m.transports.clone(),
                codecs: m.codecs.clone(),
                state: m.state,
                subscribers: m.subscribers,
                subscriber_nodes: HashSet::new(),
                updated_at: m.updated_at,
            },
        );
    }

    /// 按复合键直查端点条目（投影 / 解析用；行为对象单一真源在本表）。
    pub fn endpoint_entry(&self, key: EndpointRef) -> Option<&EndpointEntry> {
        self.endpoints.get(&key)
    }

    /// 端点行为对象（`on_subscribed` 出锁调用用；持锁调用会死锁）。
    pub fn endpoint_arc(&self, key: EndpointRef) -> Option<Arc<dyn ShareEndpoint>> {
        self.endpoints.get(&key).map(|e| e.ep.clone())
    }

    /// 端点目标类型（缺省传输选择用）。
    pub fn target(&self, key: EndpointRef) -> Option<TargetKind> {
        self.endpoints.get(&key).map(|e| e.ep.target())
    }

    /// 全部端点清单（`owner` 过滤：`Some` = 只看宿主为该节点的条目；`None` =
    /// 整张挂载表）。**确定性排序**：能力族固定顺序 + 端点 id——注册表持
    /// HashMap（无序），直接 `values()` 迭代会使展示顺序不一致（真实缺陷）。
    pub fn manifests_of(&self, owner: Option<NodeId>) -> Vec<EndpointManifest> {
        let mut v: Vec<EndpointManifest> = self
            .endpoints
            .iter()
            .filter(|(key, _)| owner.is_none_or(|o| key.owner == o))
            .map(|(_, e)| Self::manifest_of(e))
            .collect();
        v.sort_by(endpoint_order);
        v
    }

    /// 整张挂载表清单（平级存储视图；本机目录经 [`super::UnifiedRegistry`]
    /// 按 owner 过滤）。
    pub fn manifests(&self) -> Vec<EndpointManifest> {
        self.manifests_of(None)
    }

    /// 已通告端点清单（`owner` 过滤同上；对端目录用，Private 过滤由调用方做）。
    pub fn published_manifests_of(&self, owner: Option<NodeId>) -> Vec<EndpointManifest> {
        let mut v: Vec<EndpointManifest> = self
            .endpoints
            .iter()
            .filter(|(key, e)| owner.is_none_or(|o| key.owner == o) && e.published)
            .map(|(_, e)| Self::manifest_of(e))
            .collect();
        v.sort_by(endpoint_order);
        v
    }

    /// 整张挂载表已通告清单。
    pub fn published_manifests(&self) -> Vec<EndpointManifest> {
        self.published_manifests_of(None)
    }

    /// mDNS 摘要（L1）：全部端点（含不可挂载 + 未通告标记；`owner` 过滤同上）。
    pub fn summaries_of(&self, owner: Option<NodeId>) -> Vec<EndpointSummary> {
        let mut v: Vec<EndpointSummary> = self
            .endpoints
            .iter()
            .filter(|(key, _)| owner.is_none_or(|o| key.owner == o))
            .map(|(_, e)| {
                let id = e.ep.id();
                EndpointSummary {
                    endpoint_id: id.id,
                    kind: id.kind,
                    name: e.ep.name().to_string(),
                    available: e.ep.available(),
                    published: e.published,
                }
            })
            .collect();
        v.sort_by(|a, b| {
            kind_rank(a.kind)
                .cmp(&kind_rank(b.kind))
                .then_with(|| a.endpoint_id.cmp(&b.endpoint_id))
        });
        v
    }

    /// 整张挂载表 mDNS 摘要。
    pub fn summaries(&self) -> Vec<EndpointSummary> {
        self.summaries_of(None)
    }

    fn manifest_of(entry: &EndpointEntry) -> EndpointManifest {
        let strategy = entry
            .strategies
            .first()
            .cloned()
            .unwrap_or_else(|| entry.ep.strategy());
        let id = entry.ep.id();
        EndpointManifest {
            endpoint_id: id.id,
            kind: id.kind,
            name: entry.ep.name().to_string(),
            available: entry.ep.available(),
            last_error: entry.ep.last_error().map(str::to_string),
            published: entry.published,
            visibility: entry.visibility.clone(),
            delivery: entry.delivery,
            transports: entry.transports.clone(),
            // 通信模式 v2 档案：端点声明（Endpoint 契约自主指定，不按 TargetKind
            // 推导）；协商时随清单上报
            transport_profile: entry.ep.transport_profile(),
            // v2 策略组合：注册表只记录「数据包怎么处理」两要素（序列化 + pick）。
            // 平铺 pick_rule 保留为默认策略的协商摘要（wire 兼容旧对端）。
            pick_rule: strategy.pick,
            strategies: if entry.strategies.is_empty() {
                vec![strategy]
            } else {
                entry.strategies.clone()
            },
            codecs: entry.codecs.clone(),
            state: entry.state,
            subscribers: entry.subscribers,
            updated_at: entry.updated_at,
        }
    }

    /// 端点清单（订阅握手 / 目录 API 用；按复合键直查，任意宿主）。
    pub fn manifest(&self, key: EndpointRef) -> Option<EndpointManifest> {
        self.endpoints.get(&key).map(Self::manifest_of)
    }

    /// 通告端点（设置可见性 / delivery / 传输；不可挂载端点拒绝）。
    pub fn publish(
        &mut self,
        key: EndpointRef,
        visibility: Visibility,
        delivery: Delivery,
        transports: Vec<TransportPreference>,
        codecs: Vec<CodecId>,
    ) -> Result<EndpointManifest> {
        let entry = self
            .endpoints
            .get_mut(&key)
            .ok_or_else(|| Error::Message(format!("端点不存在: {key}")))?;
        if !entry.ep.available() {
            let reason = entry.ep.last_error().unwrap_or("未知原因").to_string();
            return Err(Error::Message(format!("端点不可挂载（{reason}）: {key}")));
        }
        if entry.published {
            return Err(Error::Message(format!("端点已通告: {key}")));
        }
        entry.published = true;
        entry.visibility = visibility;
        entry.delivery = delivery;
        entry.transports = transports;
        entry.codecs = codecs;
        entry.updated_at = unix_secs();
        Ok(Self::manifest_of(entry))
    }

    /// 取消通告（端点保留在表里：可再次通告；文件端点顺带移除文件源登记）。
    pub fn unpublish(&mut self, key: EndpointRef) -> Result<()> {
        let entry = self
            .endpoints
            .get_mut(&key)
            .ok_or_else(|| Error::Message(format!("端点不存在: {key}")))?;
        if !entry.published {
            return Err(Error::Message(format!("端点未通告: {key}")));
        }
        entry.published = false;
        entry.visibility = Visibility::Public;
        entry.delivery = Delivery::Pull;
        entry.transports = vec![];
        entry.codecs = vec![];
        entry.state = EndpointState::Idle;
        entry.subscribers = 0;
        entry.updated_at = unix_secs();
        self.file_sources.remove(&key);
        Ok(())
    }

    /// 公开一个本地文件为文件端点（确定目标，动态端点 `file:<名>`，重名加序号）。
    ///
    /// 返回的清单里 `kind == File`；本地路径登记进端点对象与 `file_sources`
    /// （绝不出现在摘录 / 目录 / wire）。`owner` = 宿主节点（本机端点走
    /// 统一注册表时传本机节点 id）——注册键 = `(owner, 分配的文件端点 id)`。
    pub fn publish_file(
        &mut self,
        path: &Path,
        visibility: Visibility,
        delivery: Delivery,
        owner: NodeId,
    ) -> Result<EndpointManifest> {
        let meta = std::fs::metadata(path)
            .map_err(|e| Error::Message(format!("文件不可读 {}: {e}", path.display())))?;
        if !meta.is_file() {
            return Err(Error::Message(format!("不是普通文件: {}", path.display())));
        }
        let name = path
            .file_name()
            .map_or_else(|| "未命名".into(), |s| s.to_string_lossy().to_string());
        // 数值子 id：重名文件不再进 id（`file:<n>`），只影响展示名——
        // 文件名是内容（注册表 name / FileSource 登记），不进端点身份。
        let mut endpoint_id = EndpointId::new(MediaKind::File, self.next_file_id);
        while self
            .endpoints
            .contains_key(&EndpointRef::new(owner, endpoint_id))
        {
            self.next_file_id += 1;
            endpoint_id = EndpointId::new(MediaKind::File, self.next_file_id);
        }
        self.next_file_id += 1;
        let key = EndpointRef::new(owner, endpoint_id);
        let size = meta.len();
        let ep = FileEndpoint::new(endpoint_id, name.clone(), path.to_path_buf());
        if !self.seed_with_owner(Box::new(ep), owner) {
            return Err(Error::Message(format!("端点已存在: {endpoint_id}")));
        }
        self.file_sources.insert(
            key,
            FileSource {
                path: path.to_path_buf(),
                name,
                size,
            },
        );
        self.publish(
            key,
            visibility,
            delivery,
            Self::default_transports(TargetKind::Determined),
            vec![], // 文件无编解码
        )
    }

    /// 文件端点的本地文件源（control.rs 状态展示；非文件端点返回 `None`）。
    pub fn file_source(&self, key: EndpointRef) -> Option<&FileSource> {
        self.file_sources.get(&key)
    }

    /// 更新端点运行状态（Idle/Active/Suspended + 订阅数）。
    pub fn set_state(&mut self, key: EndpointRef, state: EndpointState, subscribers: u32) -> bool {
        let Some(entry) = self.endpoints.get_mut(&key) else {
            return false;
        };
        entry.state = state;
        entry.subscribers = subscribers;
        entry.updated_at = unix_secs();
        true
    }

    /// 记录一个订阅者（订阅达成时调用）：加入端点订阅节点集并同步
    /// `subscribers` 计数（即时反映「N 订阅中」，早于数据面 watchers 事件）。
    pub fn note_subscriber(&mut self, key: EndpointRef, node_id: NodeId) {
        let Some(entry) = self.endpoints.get_mut(&key) else {
            return;
        };
        entry.subscriber_nodes.insert(node_id);
        entry.subscribers = entry.subscriber_nodes.len() as u32;
        entry.updated_at = unix_secs();
    }

    /// 记录一个取消订阅者（显式订阅终止通知时调用）：从订阅节点集移除并
    /// 同步计数；返回移除后仍存活的订阅者数（0 = 最后一个订阅者离开）。
    pub fn note_unsubscriber(&mut self, key: EndpointRef, node_id: NodeId) -> u32 {
        let Some(entry) = self.endpoints.get_mut(&key) else {
            return 0;
        };
        entry.subscriber_nodes.remove(&node_id);
        entry.subscribers = entry.subscriber_nodes.len() as u32;
        entry.updated_at = unix_secs();
        entry.subscribers
    }

    /// 清空端点订阅者集（停流 / 流结束时调用）：订阅数归零。
    pub fn clear_subscribers(&mut self, key: EndpointRef) {
        let Some(entry) = self.endpoints.get_mut(&key) else {
            return;
        };
        entry.subscriber_nodes.clear();
        entry.subscribers = 0;
    }

    /// 订阅达成事件：出锁克隆端点对象后调用其 `share`（端点自驱动，
    /// 内核不做类型分派）。注意：调用方切勿持有本注册表锁。
    /// v3 §3.2：`share` 收 `Arc<dyn ShareHost>` + `Arc<dyn Runtime>` 两个能力对象，
    /// 内核自身同时实现二者，同一 `Arc<Self>` 各取所需。
    ///
    /// 返回是否找到并触发了端点（`false` = 端点未登记，调用方可用自己持有的
    /// 端点对象兜底触发——契约 `ShareService::on_subscribed` 用它）。
    pub fn on_subscribed(&self, app: &Arc<Kernel>, key: EndpointRef, ctx: &SubscribeCtx) -> bool {
        let Some(ep) = self.endpoint_arc(key) else {
            return false;
        };
        let host: Arc<dyn ShareHost> = app.clone();
        let runtime: Arc<dyn Runtime> = app.clone();
        ep.share(host, runtime, ctx.clone());
        true
    }

    /// 端点显式订阅节点集（`subscriber_nodes` 投影真源；`note_subscriber` /
    /// `note_unsubscriber` 维护）。
    pub fn subscriber_nodes(&self, key: EndpointRef) -> Option<Vec<NodeId>> {
        self.endpoints
            .get(&key)
            .map(|e| e.subscriber_nodes.iter().copied().collect())
    }

    /// 端点默认传输（按目标类型，ReliabilityProfile 契约）：
    /// 实时目标（Lossy/Adaptive）→ QUIC > SRT > WS；确定目标（Lossless）→
    /// QUIC > WS。**不再按 MediaKind 枚举匹配**——新增端点类型按目标类型
    /// 自动获得正确策略。
    pub fn default_transports(target: TargetKind) -> Vec<TransportPreference> {
        let p = |transport: TransportId, priority: u8| TransportPreference {
            transport,
            priority,
        };
        match target {
            TargetKind::Live => vec![
                p(TransportId::Quic, 0),
                p(TransportId::Srt, 1),
                p(TransportId::Ws, 2),
            ],
            TargetKind::Determined => vec![p(TransportId::Quic, 0), p(TransportId::Ws, 1)],
        }
    }
}

/// 远端端点存根（目录登记承载，v3 §3.2）：只持有身份与展示元数据——
/// 远端端点**无本机行为对象**（不可本机 `share`/`load`），`share` 为 no-op
/// （本机只会被「对端订阅本机端点」触发，远端存根永不被本机触发）；
/// `available` 透传目录探测结果（订阅侧展示用）。
struct RemoteEndpoint {
    base: EndpointBase,
    target: TargetKind,
    transport_profile: ReliabilityProfile,
    strategy: EndpointStrategy,
}

impl stross_endpoint::contract::Endpoint for RemoteEndpoint {
    fn id(&self) -> EndpointId {
        self.base.id
    }
    fn kind(&self) -> MediaKind {
        self.base.kind
    }
    fn name(&self) -> &str {
        &self.base.name
    }
    fn target(&self) -> TargetKind {
        self.target
    }
    fn transport_profile(&self) -> ReliabilityProfile {
        self.transport_profile
    }
    fn strategy(&self) -> EndpointStrategy {
        self.strategy
    }
}

impl ShareEndpoint for RemoteEndpoint {
    fn available(&self) -> bool {
        self.base.available
    }
    fn last_error(&self) -> Option<&str> {
        self.base.last_error.as_deref()
    }
    fn load(&mut self) -> std::result::Result<(), String> {
        Ok(()) // 存根无本机探测
    }
    fn share(&self, _host: Arc<dyn ShareHost>, _runtime: Arc<dyn Runtime>, _ctx: SubscribeCtx) {
        // 远端端点不可本机 share（no-op；本机端点 share 走行为对象单一真源）
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::result::Result as StdResult;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use stross_endpoint::contract::{Endpoint, Probe};
    use stross_proto::message::MediaKind;

    fn ok_probe() -> Probe {
        Arc::new(|| Ok(()))
    }

    fn fail_probe(reason: &'static str) -> Probe {
        let r = reason.to_string();
        Arc::new(move || Err(r.clone()))
    }

    fn screen() -> Box<dyn ShareEndpoint> {
        Box::new(stross_endpoint::share::screen::ScreenEndpoint::new(
            "屏幕",
            ok_probe(),
        ))
    }

    const fn sid(kind: MediaKind, id: u32) -> EndpointId {
        EndpointId::new(kind, id)
    }

    /// 测试用复合键（seed 默认 owner = NIL，故测试查表统一用 (NIL, id)）。
    const fn ref_of(id: EndpointId) -> EndpointRef {
        EndpointRef::new(NodeId::NIL, id)
    }

    #[test]
    fn seed_loads_and_marks_availability() {
        let mut r = EndpointRegistry::new();
        // 可用端点：load 成功 → available
        assert!(r.seed(screen()));
        let m = r.manifest(ref_of(sid(MediaKind::Screen, 0))).unwrap();
        assert!(m.available);
        assert!(m.last_error.is_none());
        assert!(!m.published, "登记后未通告");
        // 不可用端点（探测失败）：保留在表里但标记不可挂载 + 原因
        let mut r2 = EndpointRegistry::new();
        assert!(r2.seed(Box::new(
            stross_endpoint::share::screen::ScreenEndpoint::new(
                "屏幕",
                fail_probe("无图形会话（DISPLAY / WAYLAND_DISPLAY 均未设置）")
            )
        )));
        let m2 = r2.manifest(ref_of(sid(MediaKind::Screen, 0))).unwrap();
        assert!(!m2.available);
        assert_eq!(
            m2.last_error.as_deref(),
            Some("无图形会话（DISPLAY / WAYLAND_DISPLAY 均未设置）")
        );
        // 不可挂载端点拒绝通告（错误携带原因）
        assert!(
            r2.publish(
                ref_of(sid(MediaKind::Screen, 0)),
                Visibility::Public,
                Delivery::Pull,
                vec![],
                vec![]
            )
            .is_err()
        );
    }

    #[test]
    fn publish_one_to_one_state_and_unpublish() {
        let mut r = EndpointRegistry::new();
        assert!(r.seed(screen()));
        assert!(!r.seed(screen()), "重复登记不覆盖");

        let m = r
            .publish(
                ref_of(sid(MediaKind::Screen, 0)),
                Visibility::Public,
                Delivery::Pull,
                EndpointRegistry::default_transports(TargetKind::Live),
                vec![CodecId::H264],
            )
            .unwrap();
        assert_eq!(m.endpoint_id, 0);
        assert_eq!(m.kind, MediaKind::Screen);
        assert!(m.published);
        assert_eq!(m.state, EndpointState::Idle);
        assert_eq!(m.subscribers, 0);
        assert_eq!(
            m.transports[0].transport,
            TransportId::Quic,
            "实时目标默认 QUIC 优先"
        );
        // 通信模式 v2 档案：实时目标（屏幕）默认 Lossy + Realtime
        assert_eq!(
            m.transport_profile,
            stross_proto::message::ReliabilityProfile::Lossy,
            "实时目标默认允许丢包"
        );
        assert_eq!(
            m.pick_rule,
            stross_proto::message::PickRule::Realtime,
            "实时目标默认严格即时规则"
        );

        // 重复通告报错
        assert!(
            r.publish(
                ref_of(sid(MediaKind::Screen, 0)),
                Visibility::Public,
                Delivery::Pull,
                vec![],
                vec![]
            )
            .is_err()
        );
        // 未知端点报错
        assert!(
            r.publish(
                ref_of(sid(MediaKind::Service, 99)),
                Visibility::Public,
                Delivery::Pull,
                vec![],
                vec![]
            )
            .is_err()
        );

        // 状态与订阅数
        assert!(r.set_state(ref_of(sid(MediaKind::Screen, 0)), EndpointState::Active, 2));
        let m = r.manifest(ref_of(sid(MediaKind::Screen, 0))).unwrap();
        assert_eq!(m.state, EndpointState::Active);
        assert_eq!(m.subscribers, 2);
        assert!(!r.set_state(
            ref_of(sid(MediaKind::Service, 99)),
            EndpointState::Active,
            0
        ));

        // 摘要携带 available + published
        let s = r.summaries();
        assert!(s.iter().any(|e| e.published && e.available));

        // 取消通告（端点保留，可再次通告）
        assert!(r.unpublish(ref_of(sid(MediaKind::Screen, 0))).is_ok());
        assert!(r.unpublish(ref_of(sid(MediaKind::Screen, 0))).is_err());
        assert!(
            !r.manifest(ref_of(sid(MediaKind::Screen, 0)))
                .unwrap()
                .published
        );
        assert!(
            r.publish(
                ref_of(sid(MediaKind::Screen, 0)),
                Visibility::Public,
                Delivery::Pull,
                vec![],
                vec![]
            )
            .is_ok()
        );
    }

    #[test]
    fn default_transports_by_target() {
        let live = EndpointRegistry::default_transports(TargetKind::Live);
        assert_eq!(
            live.iter().map(|t| t.transport).collect::<Vec<_>>(),
            vec![TransportId::Quic, TransportId::Srt, TransportId::Ws]
        );
        let determined = EndpointRegistry::default_transports(TargetKind::Determined);
        assert_eq!(
            determined.iter().map(|t| t.transport).collect::<Vec<_>>(),
            vec![TransportId::Quic, TransportId::Ws]
        );
    }

    #[test]
    fn publish_file_registers_source_and_unpublish_clears() {
        let dir = std::env::temp_dir().join(format!("stross-reg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("备注.txt");
        std::fs::write(&path, b"hello stross").unwrap();
        let mut r = EndpointRegistry::new();
        let m = r
            .publish_file(&path, Visibility::Public, Delivery::Pull, NodeId::NIL)
            .expect("公开文件端点");
        assert_eq!(m.kind, MediaKind::File);
        assert_eq!(m.name, "备注.txt", "文件名进注册表 name，不进端点身份");
        assert_eq!(m.endpoint_id, 0, "文件数值子 id 从 0 起");
        assert!(m.available, "文件端点 load 应探测可读");
        assert_eq!(m.transports.len(), 2, "确定目标默认 QUIC>WS");
        assert_eq!(m.transports[0].transport, TransportId::Quic);
        // 通信模式 v2 档案：确定目标（文件）默认 Lossless + StrictOrdered
        assert_eq!(
            m.transport_profile,
            stross_proto::message::ReliabilityProfile::Lossless,
            "确定目标默认不允许丢包"
        );
        assert_eq!(
            m.pick_rule,
            stross_proto::message::PickRule::StrictOrdered,
            "确定目标默认严格顺序规则"
        );
        let m_id = ref_of(EndpointId::new(m.kind, m.endpoint_id));
        // 文件源可查（本地路径不落 wire：清单里没有 path 字段）
        let src = r.file_source(m_id).expect("文件源已登记");
        assert_eq!(src.name, "备注.txt");
        assert_eq!(src.size, b"hello stross".len() as u64);
        // 重名文件数值子 id 递增（`file:0` / `file:1`）
        let m2 = r
            .publish_file(&path, Visibility::Public, Delivery::Pull, NodeId::NIL)
            .unwrap();
        assert_ne!(m.endpoint_id, m2.endpoint_id);
        assert_eq!(m2.endpoint_id, 1);
        // 摘要含动态端点
        assert!(r.summaries().iter().any(|e| e.kind == MediaKind::File));
        // 取消通告 → 文件源移除、published 归 false（端点保留）
        r.unpublish(m_id).unwrap();
        assert!(r.file_source(m_id).is_none());
        assert!(!r.manifest(m_id).unwrap().published);
        r.unpublish(ref_of(EndpointId::new(m2.kind, m2.endpoint_id)))
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn on_subscribed_calls_endpoint_share_outside_lock() {
        // 端点自驱动：订阅事件 → share 被调用（内核不分派）
        let fired = Arc::new(AtomicUsize::new(0));
        let f = fired.clone();
        struct CountingEndpoint {
            base: stross_endpoint::contract::EndpointBase,
            fired: Arc<AtomicUsize>,
        }
        impl Endpoint for CountingEndpoint {
            fn id(&self) -> EndpointId {
                self.base.id
            }
            fn kind(&self) -> MediaKind {
                self.base.kind
            }
            fn name(&self) -> &str {
                &self.base.name
            }
            fn target(&self) -> TargetKind {
                TargetKind::Live
            }
            fn transport_profile(&self) -> stross_proto::message::ReliabilityProfile {
                stross_proto::message::ReliabilityProfile::Lossy
            }
            fn strategy(&self) -> stross_proto::message::EndpointStrategy {
                stross_proto::message::EndpointStrategy {
                    strategy_id: stross_proto::message::EndpointStrategy::DEFAULT_ID,
                    serialize: stross_proto::message::SerializeRule::Passthrough,
                    pick: stross_proto::message::PickRule::Realtime,
                }
            }
        }
        impl ShareEndpoint for CountingEndpoint {
            fn available(&self) -> bool {
                self.base.available
            }
            fn last_error(&self) -> Option<&str> {
                self.base.last_error.as_deref()
            }
            fn load(&mut self) -> StdResult<(), String> {
                self.base.available = true;
                Ok(())
            }
            fn share(
                &self,
                _host: Arc<dyn stross_endpoint::contract::ShareHost>,
                _runtime: Arc<dyn stross_endpoint::contract::Runtime>,
                ctx: SubscribeCtx,
            ) {
                assert_eq!(ctx.subscriber, NodeId::from("dev-phone"));
                self.fired.fetch_add(1, Ordering::SeqCst);
            }
        }
        let mut r = EndpointRegistry::new();
        r.seed(Box::new(CountingEndpoint {
            base: stross_endpoint::contract::EndpointBase {
                id: EndpointId::new(MediaKind::Mic, 0),
                kind: MediaKind::Mic,
                name: "录音".into(),
                available: false,
                last_error: None,
            },
            fired: f,
        }));
        r.publish(
            ref_of(EndpointId::new(MediaKind::Mic, 0)),
            Visibility::Confirm,
            Delivery::Push,
            vec![],
            vec![],
        )
        .unwrap();
        let ctx = SubscribeCtx {
            subscriber: "dev-phone".into(),
            delivery: Delivery::Push,
            stream_id: "sess-1".into(),
            transport_profile: stross_proto::message::ReliabilityProfile::Lossy,
            strategy: stross_proto::message::EndpointStrategy {
                strategy_id: stross_proto::message::EndpointStrategy::DEFAULT_ID,
                serialize: stross_proto::message::SerializeRule::Passthrough,
                pick: stross_proto::message::PickRule::Realtime,
            },
            relay_addr: Some("ws://192.168.1.5:9000".into()),
            share_token: Some("tok".into()),
        };
        let app = Arc::new(Kernel::new(crate::Platform::Desktop));
        r.on_subscribed(&app, ref_of(EndpointId::new(MediaKind::Mic, 0)), &ctx);
        assert_eq!(fired.load(Ordering::SeqCst), 1);
        // 未知端点不触发
        r.on_subscribed(&app, ref_of(EndpointId::new(MediaKind::Service, 99)), &ctx);
        assert_eq!(fired.load(Ordering::SeqCst), 1);
    }
}
