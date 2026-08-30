//! 端点注册表：**三层统一注册表**（节点 → 端点 → 策略）+ 通告参数管理。
//!
//! 设计规格：docs/endpoint-model-v2.md（v2 演进，取代 v1 单层模型；
//! v1 已删除（历史见 git））。
//!
//! * 端点 = 节点上可共享的能力实体；**契约（[`Endpoint`] / [`SubscribeCtx`] /
//!   [`Probe`] 等）与具体端点实现（屏幕 / 麦克风 / 系统声音 / 文件）在
//!   [`stross_endpoint`] 插件区**，本模块只做身份登记、通告参数管理与订阅联动
//!   （内核 = 纯管理调度，不做媒体数据面）；
//! * 端点自维护「可挂载性」（`available`，load 探测结果）与失败原因
//!   （`last_error`）；注册表只做身份登记与通告参数管理；
//! * **统一注册表**（[`UnifiedRegistry`]）：本机（[`EndpointRegistry`] 行为对象
//!   表）与互联节点（目录/发现映射）都在**同一张表**里——订阅统一按
//!   `(节点 id, 端点 id, 策略 id) → 策略组合` 查表，自订与订其它互联节点
//!   走同一套逻辑；策略 = 序列化规则 + pick 规则（[`EndpointStrategy`]），
//!   传输档案不进注册表；
//! * **订阅联动**：`on_subscribed` 出锁克隆端点对象后调用其 `share`
//!   （端点自驱动，内核不做类型分派）；订阅端由
//!   [`UnifiedRegistry::generate_subscribe_endpoint`] 生成（订阅端点生成）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use stross_endpoint::contract::{
    EndpointClass, ShareEndpoint, SubscribeCtx, SubscribeEndpoint, TargetKind,
};
use stross_endpoint::share::file::FileEndpoint;
use stross_endpoint::subscribe::file::FileReceiveEndpoint;
use stross_endpoint::subscribe::media::MediaReceiveEndpoint;
use stross_proto::message::{
    CodecId, Delivery, EndpointDir, EndpointManifest, EndpointState, EndpointStrategy,
    EndpointSummary, PickRule, ReliabilityProfile, SerializeRule, StrategyId, SubscribeSpec,
    TransportId, TransportPreference, Visibility,
};
use stross_proto::time::unix_secs;

use crate::Kernel;
use crate::error::{Error, Result};

/// 端点条目：行为对象（[`Endpoint`]）+ 通告参数（公开者声明）。
pub struct EndpointEntry {
    pub ep: Arc<dyn ShareEndpoint>,
    pub published: bool,
    pub visibility: Visibility,
    pub delivery: Delivery,
    pub transports: Vec<TransportPreference>,
    pub codecs: Vec<CodecId>,
    pub state: EndpointState,
    pub subscribers: u32,
    pub updated_at: u64,
}

/// 端点注册表：**本机（自节点）端点表**——行为对象 + 通告参数。
///
/// v2 三层注册表中本机节点这一层的承载（[`UnifiedRegistry`] 持有它）；
/// 端点自维护可挂载性（`load` 探测）；注册表只做身份登记与通告参数管理。
#[derive(Default)]
pub struct EndpointRegistry {
    endpoints: HashMap<String, EndpointEntry>,
    /// 文件端点：endpoint_id → 本地文件源（control.rs 状态展示）。
    file_sources: HashMap<String, FileSource>,
}

impl EndpointRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记端点并立即 `load`（探测可挂载性）；id 已存在时返回 `false`。
    ///
    /// load 失败不阻止登记：端点保留在表里但标记不可挂载（`available=false`
    /// + `last_error`）——UI 可见原因，不可通告/订阅。
    pub fn seed(&mut self, mut ep: Box<dyn ShareEndpoint>) -> bool {
        let id = ep.id().to_string();
        if self.endpoints.contains_key(&id) {
            return false;
        }
        if let Err(e) = ep.load() {
            tracing::warn!("端点 {id} load 失败，标记不可挂载: {e}");
        }
        self.endpoints.insert(
            id,
            EndpointEntry {
                ep: Arc::from(ep),
                published: false,
                visibility: Visibility::Public,
                delivery: Delivery::Pull,
                transports: vec![],
                codecs: vec![],
                state: EndpointState::Idle,
                subscribers: 0,
                updated_at: unix_secs(),
            },
        );
        true
    }

    /// 端点行为对象（`on_subscribed` 出锁调用用；持锁调用会死锁）。
    pub fn endpoint_arc(&self, endpoint_id: &str) -> Option<Arc<dyn ShareEndpoint>> {
        self.endpoints.get(endpoint_id).map(|e| e.ep.clone())
    }

    /// 端点目标类型（缺省传输选择用）。
    pub fn target(&self, endpoint_id: &str) -> Option<TargetKind> {
        self.endpoints.get(endpoint_id).map(|e| e.ep.target())
    }

    /// 全部端点清单（本机目录用；含未通告）。
    pub fn manifests(&self) -> Vec<EndpointManifest> {
        self.endpoints.values().map(Self::manifest_of).collect()
    }

    /// 已通告端点清单（对端目录用；Private 过滤由调用方做）。
    pub fn published_manifests(&self) -> Vec<EndpointManifest> {
        self.endpoints
            .values()
            .filter(|e| e.published)
            .map(Self::manifest_of)
            .collect()
    }

    /// mDNS 摘要（L1）：全部端点（含不可挂载 + 未通告标记）。
    pub fn summaries(&self) -> Vec<EndpointSummary> {
        self.endpoints
            .values()
            .map(|e| EndpointSummary {
                endpoint_id: e.ep.id().to_string(),
                kind: e.ep.kind(),
                name: e.ep.name().to_string(),
                available: e.ep.available(),
                published: e.published,
            })
            .collect()
    }

    fn manifest_of(entry: &EndpointEntry) -> EndpointManifest {
        let strategy = entry.ep.strategy();
        EndpointManifest {
            endpoint_id: entry.ep.id().to_string(),
            kind: entry.ep.kind(),
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
            strategies: vec![strategy],
            codecs: entry.codecs.clone(),
            state: entry.state,
            subscribers: entry.subscribers,
            updated_at: entry.updated_at,
        }
    }

    /// 端点清单（订阅握手 / 目录 API 用）。
    pub fn manifest(&self, endpoint_id: &str) -> Option<EndpointManifest> {
        self.endpoints.get(endpoint_id).map(Self::manifest_of)
    }

    /// 通告端点（设置可见性 / delivery / 传输；不可挂载端点拒绝）。
    pub fn publish(
        &mut self,
        endpoint_id: &str,
        visibility: Visibility,
        delivery: Delivery,
        transports: Vec<TransportPreference>,
        codecs: Vec<CodecId>,
    ) -> Result<EndpointManifest> {
        let entry = self
            .endpoints
            .get_mut(endpoint_id)
            .ok_or_else(|| Error::Message(format!("端点不存在: {endpoint_id}")))?;
        if !entry.ep.available() {
            let reason = entry.ep.last_error().unwrap_or("未知原因").to_string();
            return Err(Error::Message(format!(
                "端点不可挂载（{reason}）: {endpoint_id}"
            )));
        }
        if entry.published {
            return Err(Error::Message(format!("端点已通告: {endpoint_id}")));
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
    pub fn unpublish(&mut self, endpoint_id: &str) -> Result<()> {
        let entry = self
            .endpoints
            .get_mut(endpoint_id)
            .ok_or_else(|| Error::Message(format!("端点不存在: {endpoint_id}")))?;
        if !entry.published {
            return Err(Error::Message(format!("端点未通告: {endpoint_id}")));
        }
        entry.published = false;
        entry.visibility = Visibility::Public;
        entry.delivery = Delivery::Pull;
        entry.transports = vec![];
        entry.codecs = vec![];
        entry.state = EndpointState::Idle;
        entry.subscribers = 0;
        entry.updated_at = unix_secs();
        self.file_sources.remove(endpoint_id);
        Ok(())
    }

    /// 公开一个本地文件为文件端点（确定目标，动态端点 `file:<名>`，重名加序号）。
    ///
    /// 返回的清单里 `kind == File`；本地路径登记进端点对象与 `file_sources`
    /// （绝不出现在摘录 / 目录 / wire）。
    pub fn publish_file(
        &mut self,
        path: &Path,
        visibility: Visibility,
        delivery: Delivery,
    ) -> Result<EndpointManifest> {
        let meta = std::fs::metadata(path)
            .map_err(|e| Error::Message(format!("文件不可读 {}: {e}", path.display())))?;
        if !meta.is_file() {
            return Err(Error::Message(format!("不是普通文件: {}", path.display())));
        }
        let name = path
            .file_name()
            .map_or_else(|| "未命名".into(), |s| s.to_string_lossy().to_string());
        let mut endpoint_id = format!("file:{name}");
        let mut n = 2;
        while self.endpoints.contains_key(&endpoint_id) {
            endpoint_id = format!("file:{name}-{n}");
            n += 1;
        }
        let size = meta.len();
        let ep = FileEndpoint::new(endpoint_id.clone(), name.clone(), path.to_path_buf());
        if !self.seed(Box::new(ep)) {
            return Err(Error::Message(format!("端点已存在: {endpoint_id}")));
        }
        self.file_sources.insert(
            endpoint_id.clone(),
            FileSource {
                path: path.to_path_buf(),
                name: name.clone(),
                size,
            },
        );
        self.publish(
            &endpoint_id,
            visibility,
            delivery,
            Self::default_transports(TargetKind::Determined),
            vec![], // 文件无编解码
        )
    }

    /// 文件端点的本地文件源（control.rs 状态展示；非文件端点返回 `None`）。
    pub fn file_source(&self, endpoint_id: &str) -> Option<&FileSource> {
        self.file_sources.get(endpoint_id)
    }

    /// 更新端点运行状态（Idle/Active/Suspended + 订阅数）。
    pub fn set_state(&mut self, endpoint_id: &str, state: EndpointState, subscribers: u32) -> bool {
        let Some(entry) = self.endpoints.get_mut(endpoint_id) else {
            return false;
        };
        entry.state = state;
        entry.subscribers = subscribers;
        entry.updated_at = unix_secs();
        true
    }

    /// 订阅达成事件：出锁克隆端点对象后调用其 `share`（端点自驱动，
    /// 内核不做类型分派）。注意：调用方切勿持有本注册表锁。
    pub fn on_subscribed(&self, app: &Arc<Kernel>, endpoint_id: &str, ctx: &SubscribeCtx) {
        let Some(ep) = self.endpoint_arc(endpoint_id) else {
            return;
        };
        ep.share(app.clone(), ctx.clone());
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

/// 统一注册表（v2 三层：节点 → 端点 → 策略；docs/endpoint-model-v2.md §2）。
///
/// **所有参与互联的节点（含本机）都在这一张表**：本机端点行为对象在
/// [`EndpointRegistry`]（自节点注册），互联节点经目录/发现映射进
/// [`NodeRegistration`]——订阅统一按 `(节点 id, 端点 id, 策略 id)` 查表，
/// 自订与订其它互联节点走同一套逻辑。
pub struct UnifiedRegistry {
    /// 本机（自节点）端点表：行为对象 + 通告参数（订阅联动走它）。
    local: EndpointRegistry,
    /// 互联节点注册（目录/发现拉取后映射；不含本机——本机走 `local`）。
    nodes: HashMap<String, NodeRegistration>,
    /// 本机节点 id（身份注入；未注入时缺省 `"local"`）。
    self_node: String,
    /// 本机节点展示名（身份注入）。
    self_name: String,
}

/// 一个互联节点（手机/电脑）的注册：节点信息 + 它拥有的端点（可分享内容）。
#[derive(Debug, Clone)]
pub struct NodeRegistration {
    /// 互联节点 id（device_id；mDNS/目录权威）。
    pub node_id: String,
    /// 展示名（device_name）。
    pub name: String,
    /// 协商/目录入口（`host:port`；本机为 `"local"`）。
    pub addr: String,
    /// 是否本机（本机 = `local` 表 + 本注册镜像，便于 UI 高亮；非特殊身份）。
    pub is_self: bool,
    /// 该互联节点的下属端点。
    pub endpoints: HashMap<String, EndpointRegistration>,
}

/// 一个端点（节点上可分享的具体内容，如"屏幕"/"麦克风"）的注册。
#[derive(Debug, Clone)]
pub struct EndpointRegistration {
    pub endpoint_id: String,
    pub kind: stross_proto::message::MediaKind,
    pub name: String,
    /// 目标类型（由协商档案推断；远端不落 wire）。
    pub target: TargetKind,
    /// 端点自主声明的策略组合（策略独立可寻址，同一内容可有多种处理组合）。
    pub strategies: HashMap<StrategyId, EndpointStrategy>,
}

impl Default for UnifiedRegistry {
    fn default() -> Self {
        Self {
            local: EndpointRegistry::new(),
            nodes: HashMap::new(),
            self_node: "local".into(),
            self_name: "本机".into(),
        }
    }
}

impl UnifiedRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注入本机节点身份（`is_self` 判定与 `(节点, 端点, 策略)` 查表的
    /// 本机分支用；身份未注入时本机缺省 id 为 `"local"`）。
    pub fn set_self_node(&mut self, node_id: &str, name: &str) {
        self.self_node = node_id.to_string();
        self.self_name = name.to_string();
    }

    /// 本机节点 id（订阅查表的本机分支键）。
    pub fn self_node_id(&self) -> &str {
        &self.self_node
    }

    // -- 本机端点表委托（行为对象 + 通告参数；原 EndpointRegistry 方法面） --

    pub fn seed(&mut self, ep: Box<dyn ShareEndpoint>) -> bool {
        self.local.seed(ep)
    }

    pub fn endpoint_arc(&self, endpoint_id: &str) -> Option<Arc<dyn ShareEndpoint>> {
        self.local.endpoint_arc(endpoint_id)
    }

    pub fn target(&self, endpoint_id: &str) -> Option<TargetKind> {
        self.local.target(endpoint_id)
    }

    pub fn manifests(&self) -> Vec<EndpointManifest> {
        self.local.manifests()
    }

    pub fn published_manifests(&self) -> Vec<EndpointManifest> {
        self.local.published_manifests()
    }

    pub fn summaries(&self) -> Vec<EndpointSummary> {
        self.local.summaries()
    }

    pub fn manifest(&self, endpoint_id: &str) -> Option<EndpointManifest> {
        self.local.manifest(endpoint_id)
    }

    pub fn publish(
        &mut self,
        endpoint_id: &str,
        visibility: Visibility,
        delivery: Delivery,
        transports: Vec<TransportPreference>,
        codecs: Vec<CodecId>,
    ) -> Result<EndpointManifest> {
        self.local
            .publish(endpoint_id, visibility, delivery, transports, codecs)
    }

    pub fn unpublish(&mut self, endpoint_id: &str) -> Result<()> {
        self.local.unpublish(endpoint_id)
    }

    pub fn publish_file(
        &mut self,
        path: &Path,
        visibility: Visibility,
        delivery: Delivery,
    ) -> Result<EndpointManifest> {
        self.local.publish_file(path, visibility, delivery)
    }

    pub fn file_source(&self, endpoint_id: &str) -> Option<&FileSource> {
        self.local.file_source(endpoint_id)
    }

    pub fn set_state(&mut self, endpoint_id: &str, state: EndpointState, subscribers: u32) -> bool {
        self.local.set_state(endpoint_id, state, subscribers)
    }

    pub fn on_subscribed(&self, app: &Arc<Kernel>, endpoint_id: &str, ctx: &SubscribeCtx) {
        self.local.on_subscribed(app, endpoint_id, ctx);
    }

    pub fn default_transports(target: TargetKind) -> Vec<TransportPreference> {
        EndpointRegistry::default_transports(target)
    }

    // -- v2 三层注册：互联节点映射 + 策略解析 + 订阅端点生成 --

    /// 把目录响应（`GET /api/endpoints`）的互联节点映射进统一注册表：
    /// 节点 → 端点 → 策略（策略组合来自清单 `strategies`，缺省由平铺
    /// `pick_rule` 推导）。幂等：同节点重复拉取覆盖（目录是权威快照）。
    pub fn register_remote_directory(&mut self, dir: &EndpointDir, addr: &str) {
        let node_id = dir.node.device_id.clone();
        if node_id.is_empty() || node_id == self.self_node {
            return; // 空节点 / 本机镜像不入远端表（本机走 local）
        }
        let mut reg = NodeRegistration {
            node_id: node_id.clone(),
            name: dir.node.device_name.clone(),
            addr: addr.to_string(),
            is_self: false,
            endpoints: HashMap::new(),
        };
        for m in &dir.endpoints {
            reg.endpoints.insert(
                m.endpoint_id.clone(),
                EndpointRegistration {
                    endpoint_id: m.endpoint_id.clone(),
                    kind: m.kind,
                    name: m.name.clone(),
                    target: target_from_manifest(m),
                    strategies: strategies_of(m),
                },
            );
        }
        self.nodes.insert(node_id, reg);
    }

    /// 策略解析（统一查表）：`registry[节点][端点][策略]` → 策略组合。
    ///
    /// * 本机：从行为对象取（`strategy()` 单一真源）；
    /// * 互联节点：从目录映射取；`strategy_id` 缺省 = 端点默认策略（首个）。
    pub fn resolve_strategy(
        &self,
        node_id: &str,
        endpoint_id: &str,
        strategy_id: Option<&str>,
    ) -> Option<EndpointStrategy> {
        if node_id == self.self_node || node_id == "local" {
            let ep = self.local.endpoint_arc(endpoint_id)?;
            let s = ep.strategy();
            // 本机单策略：任何 id 都收敛到端点声明的策略
            return Some(match strategy_id {
                Some(id) if id == s.strategy_id => s,
                _ => s,
            });
        }
        let node = self.nodes.get(node_id)?;
        let ep = node.endpoints.get(endpoint_id)?;
        match strategy_id {
            Some(id) => ep.strategies.get(id).cloned(),
            None => ep.strategies.values().next().cloned(),
        }
    }

    /// 三层注册表快照（含本机镜像；UI / 调试用）：节点 → 端点 → 策略。
    pub fn node_registrations(&self) -> Vec<NodeRegistration> {
        let mut v: Vec<NodeRegistration> = self.nodes.values().cloned().collect();
        // 本机镜像：从行为对象表现算（策略 = 端点声明，单一真源）
        let mut self_reg = NodeRegistration {
            node_id: self.self_node.clone(),
            name: self.self_name.clone(),
            addr: "local".into(),
            is_self: true,
            endpoints: HashMap::new(),
        };
        for m in self.local.manifests() {
            self_reg.endpoints.insert(
                m.endpoint_id.clone(),
                EndpointRegistration {
                    endpoint_id: m.endpoint_id.clone(),
                    kind: m.kind,
                    name: m.name.clone(),
                    target: target_from_manifest(&m),
                    strategies: strategies_of(&m),
                },
            );
        }
        v.push(self_reg);
        v.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        v
    }

    /// 订阅端点生成（docs/endpoint-model-v2.md §3「订阅端点生成」）：
    /// 按订阅目标端点的**能力族**（[`EndpointClass`]：Graph/Audio/File…）
    /// 构造**统一的族订阅端点**（内核不做类型分派——端点实现自驱动，
    /// 与分享端 `share` 同构）。
    ///
    /// * `File` → 文件订阅端（接收落盘到 `out_dir`）；
    /// * `Graph` / `Audio` → 媒体订阅端（[`MediaReceiveEndpoint`]：收流 +
    ///   解码，播放器入端点；`out_dir` 不适用）；
    /// * 其余族（剪贴板/输入/服务）暂未定义订阅端点宿主 → `None`。
    pub fn generate_subscribe_endpoint(
        &self,
        spec: &SubscribeSpec,
        out_dir: Option<&Path>,
    ) -> Option<Box<dyn SubscribeEndpoint>> {
        let (kind, class) = self
            .nodes
            .get(&spec.node_id)
            .and_then(|n| n.endpoints.get(&spec.endpoint_id))
            .map(|e| (e.kind, EndpointClass::from_kind(e.kind)))
            .or_else(|| {
                self.local
                    .manifest(&spec.endpoint_id)
                    .map(|m| (m.kind, EndpointClass::from_kind(m.kind)))
            })?;
        match class {
            EndpointClass::File => Some(Box::new(FileReceiveEndpoint::new(
                format!("recv:{}", spec.endpoint_id),
                spec.endpoint_id.clone(),
                out_dir
                    .map(Path::to_path_buf)
                    .unwrap_or_else(std::env::temp_dir),
            ))),
            EndpointClass::Graph | EndpointClass::Audio => {
                Some(Box::new(MediaReceiveEndpoint::new(
                    format!("recv:{}", spec.endpoint_id),
                    spec.endpoint_id.clone(),
                    kind,
                )))
            }
            // 剪贴板 / 输入 / 服务：暂无订阅端点宿主（后续按族补实现）
            EndpointClass::Clipboard | EndpointClass::Input | EndpointClass::Service => None,
        }
    }

    /// 全部互联节点 id（订阅查表键；含本机）。
    pub fn node_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.nodes.keys().cloned().collect();
        ids.push(self.self_node.clone());
        ids.sort();
        ids
    }
}

/// 从清单推导目标类型（远端目标类型不落 wire；按协商档案推断——
/// Lossless + StrictOrdered 为确定目标，其余为实时目标）。
fn target_from_manifest(m: &EndpointManifest) -> TargetKind {
    if m.transport_profile == ReliabilityProfile::Lossless && m.pick_rule == PickRule::StrictOrdered
    {
        TargetKind::Determined
    } else {
        TargetKind::Live
    }
}

/// 清单 → 策略组合表（缺省由平铺 `pick_rule` 推导直通 + pick 的默认策略）。
fn strategies_of(m: &EndpointManifest) -> HashMap<StrategyId, EndpointStrategy> {
    if !m.strategies.is_empty() {
        return m
            .strategies
            .iter()
            .map(|s| (s.strategy_id.clone(), s.clone()))
            .collect();
    }
    let mut h = HashMap::new();
    h.insert(
        EndpointStrategy::DEFAULT_ID.into(),
        EndpointStrategy {
            strategy_id: EndpointStrategy::DEFAULT_ID.into(),
            serialize: SerializeRule::Passthrough,
            pick: m.pick_rule,
        },
    );
    h
}

/// 文件端点本地文件源（`control.rs` 状态展示用；路径不落 wire）。
#[derive(Debug, Clone)]
pub struct FileSource {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
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

    #[test]
    fn seed_loads_and_marks_availability() {
        let mut r = EndpointRegistry::new();
        // 可用端点：load 成功 → available
        assert!(r.seed(screen()));
        let m = r.manifest("screen:0").unwrap();
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
        let m2 = r2.manifest("screen:0").unwrap();
        assert!(!m2.available);
        assert_eq!(
            m2.last_error.as_deref(),
            Some("无图形会话（DISPLAY / WAYLAND_DISPLAY 均未设置）")
        );
        // 不可挂载端点拒绝通告（错误携带原因）
        assert!(
            r2.publish(
                "screen:0",
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
                "screen:0",
                Visibility::Public,
                Delivery::Pull,
                EndpointRegistry::default_transports(TargetKind::Live),
                vec![CodecId::H264],
            )
            .unwrap();
        assert_eq!(m.endpoint_id, "screen:0");
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
                "screen:0",
                Visibility::Public,
                Delivery::Pull,
                vec![],
                vec![]
            )
            .is_err()
        );
        // 未知端点报错
        assert!(
            r.publish("nope", Visibility::Public, Delivery::Pull, vec![], vec![])
                .is_err()
        );

        // 状态与订阅数
        assert!(r.set_state("screen:0", EndpointState::Active, 2));
        let m = r.manifest("screen:0").unwrap();
        assert_eq!(m.state, EndpointState::Active);
        assert_eq!(m.subscribers, 2);
        assert!(!r.set_state("nope", EndpointState::Active, 0));

        // 摘要携带 available + published
        let s = r.summaries();
        assert!(s.iter().any(|e| e.published && e.available));

        // 取消通告（端点保留，可再次通告）
        assert!(r.unpublish("screen:0").is_ok());
        assert!(r.unpublish("screen:0").is_err());
        assert!(!r.manifest("screen:0").unwrap().published);
        assert!(
            r.publish(
                "screen:0",
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
            .publish_file(&path, Visibility::Public, Delivery::Pull)
            .expect("公开文件端点");
        assert_eq!(m.kind, MediaKind::File);
        assert!(
            m.endpoint_id.starts_with("file:备注.txt"),
            "{}",
            m.endpoint_id
        );
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
        // 文件源可查（本地路径不落 wire：清单里没有 path 字段）
        let src = r.file_source(&m.endpoint_id).expect("文件源已登记");
        assert_eq!(src.name, "备注.txt");
        assert_eq!(src.size, b"hello stross".len() as u64);
        // 重名自动加序号
        let m2 = r
            .publish_file(&path, Visibility::Public, Delivery::Pull)
            .unwrap();
        assert_ne!(m.endpoint_id, m2.endpoint_id);
        // 摘要含动态端点
        assert!(r.summaries().iter().any(|e| e.kind == MediaKind::File));
        // 取消通告 → 文件源移除、published 归 false（端点保留）
        r.unpublish(&m.endpoint_id).unwrap();
        assert!(r.file_source(&m.endpoint_id).is_none());
        assert!(!r.manifest(&m.endpoint_id).unwrap().published);
        assert!(r.unpublish(&m2.endpoint_id).is_ok());
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
            fn id(&self) -> &str {
                &self.base.id
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
                    strategy_id: stross_proto::message::EndpointStrategy::DEFAULT_ID.into(),
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
                _app: Arc<dyn stross_endpoint::contract::EndpointApp>,
                ctx: SubscribeCtx,
            ) {
                assert_eq!(ctx.subscriber, "dev-phone");
                self.fired.fetch_add(1, Ordering::SeqCst);
            }
        }
        let mut r = EndpointRegistry::new();
        r.seed(Box::new(CountingEndpoint {
            base: stross_endpoint::contract::EndpointBase {
                id: "rec:0".into(),
                kind: MediaKind::Mic,
                name: "录音".into(),
                available: false,
                last_error: None,
            },
            fired: f.clone(),
        }));
        r.publish("rec:0", Visibility::Confirm, Delivery::Push, vec![], vec![])
            .unwrap();
        let ctx = SubscribeCtx {
            subscriber: "dev-phone".into(),
            delivery: Delivery::Push,
            stream_id: "sess-1".into(),
            transport_profile: stross_proto::message::ReliabilityProfile::Lossy,
            strategy: stross_proto::message::EndpointStrategy {
                strategy_id: stross_proto::message::EndpointStrategy::DEFAULT_ID.into(),
                serialize: stross_proto::message::SerializeRule::Passthrough,
                pick: stross_proto::message::PickRule::Realtime,
            },
            relay_addr: Some("ws://192.168.1.5:9000".into()),
            share_token: Some("tok".into()),
        };
        let app = Arc::new(Kernel::new(crate::Platform::Desktop));
        r.on_subscribed(&app, "rec:0", &ctx);
        assert_eq!(fired.load(Ordering::SeqCst), 1);
        // 未知端点不触发
        r.on_subscribed(&app, "nope", &ctx);
        assert_eq!(fired.load(Ordering::SeqCst), 1);
    }

    // -----------------------------------------------------------------------
    // v2 三层统一注册表（docs/endpoint-model-v2.md §2）
    // -----------------------------------------------------------------------

    fn remote_dir(node_id: &str, name: &str) -> EndpointDir {
        EndpointDir {
            node: stross_proto::message::EndpointNode {
                device_id: node_id.into(),
                device_name: name.into(),
            },
            endpoints: vec![
                EndpointManifest {
                    endpoint_id: "screen:0".into(),
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
                    strategies: vec![stross_proto::message::EndpointStrategy {
                        strategy_id: stross_proto::message::EndpointStrategy::DEFAULT_ID.into(),
                        serialize: stross_proto::message::SerializeRule::Passthrough,
                        pick: stross_proto::message::PickRule::Realtime,
                    }],
                    codecs: vec![CodecId::H264],
                    state: EndpointState::Idle,
                    subscribers: 0,
                    updated_at: unix_secs(),
                },
                EndpointManifest {
                    endpoint_id: "file:notes.txt".into(),
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
                    strategies: vec![stross_proto::message::EndpointStrategy {
                        strategy_id: stross_proto::message::EndpointStrategy::DEFAULT_ID.into(),
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

    /// 三层注册表：本机与互联节点同一张表，订阅按 (节点, 端点, 策略) 统一查表。
    #[test]
    fn unified_registry_three_layer_lookup() {
        let mut reg = UnifiedRegistry::new();
        reg.set_self_node("node-pc", "电脑");
        assert!(reg.seed(screen()), "本机端点登记（自节点注册）");
        assert!(reg.seed(Box::new(FileEndpoint::new(
            "file:本地.txt".into(),
            "本地.txt".into(),
            std::env::temp_dir().join("stross-unified.txt"),
        ))));

        // 本机查表：registry[本机][端点][策略] → 策略组合（strategy() 单一真源）
        let s = reg
            .resolve_strategy("node-pc", "screen:0", None)
            .expect("本机屏幕端点应可解析");
        assert_eq!(s.strategy_id, "default");
        assert_eq!(s.pick, stross_proto::message::PickRule::Realtime);
        assert_eq!(
            s.serialize,
            stross_proto::message::SerializeRule::Passthrough
        );
        // 本机文件端点：严格顺序 + Lossless 推断为确定目标
        let fs = reg
            .resolve_strategy("node-pc", "file:本地.txt", Some("default"))
            .expect("本机文件端点应可解析");
        assert_eq!(fs.pick, stross_proto::message::PickRule::StrictOrdered);
        // 未知端点 → None
        assert!(reg.resolve_strategy("node-pc", "nope", None).is_none());

        // 互联节点映射（目录拉取 → 节点 → 端点 → 策略）
        reg.register_remote_directory(&remote_dir("node-phone", "手机A"), "192.168.1.5:18779");
        let s = reg
            .resolve_strategy("node-phone", "screen:0", None)
            .expect("远端屏幕端点应可解析");
        assert_eq!(s.pick, stross_proto::message::PickRule::Realtime);
        let f = reg
            .resolve_strategy("node-phone", "file:notes.txt", Some("default"))
            .expect("远端文件端点应可解析");
        assert_eq!(f.pick, stross_proto::message::PickRule::StrictOrdered);
        // 未知策略 id → None（策略独立可寻址）
        assert!(
            reg.resolve_strategy("node-phone", "screen:0", Some("nope"))
                .is_none()
        );

        // 快照：本机 + 互联节点都在同一张表（含 is_self 标记）
        let nodes = reg.node_registrations();
        assert_eq!(nodes.len(), 2, "本机 + 手机两台节点");
        let self_node = nodes.iter().find(|n| n.is_self).expect("本机镜像在表内");
        assert_eq!(self_node.node_id, "node-pc");
        assert!(self_node.endpoints.contains_key("screen:0"));
        let phone = nodes
            .iter()
            .find(|n| n.node_id == "node-phone")
            .expect("手机节点在表内");
        assert!(!phone.is_self);
        assert_eq!(phone.endpoints.len(), 2);

        // 订阅端点生成（按能力族分发）：File → 文件订阅端（接收落盘）；
        // Graph/Audio → 媒体订阅端（播放器入端点）
        let spec = SubscribeSpec {
            node_id: "node-phone".into(),
            endpoint_id: "file:notes.txt".into(),
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
            endpoint_id: "screen:0".into(),
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
    /// 三层查表仍可用（旧对端兼容）。
    #[test]
    fn unified_registry_remote_without_strategies_derives_default() {
        let mut reg = UnifiedRegistry::new();
        reg.set_self_node("node-pc", "电脑");
        let mut dir = remote_dir("node-old", "旧对端");
        dir.endpoints[0].strategies = vec![]; // 旧 wire：无策略列表
        dir.endpoints[0].pick_rule = stross_proto::message::PickRule::Realtime;
        reg.register_remote_directory(&dir, "192.168.1.9:18779");
        let s = reg
            .resolve_strategy("node-old", "screen:0", None)
            .expect("旧对端策略应推导成功");
        assert_eq!(s.strategy_id, "default");
        assert_eq!(s.pick, stross_proto::message::PickRule::Realtime);
        assert_eq!(
            s.serialize,
            stross_proto::message::SerializeRule::Passthrough
        );
    }
}
