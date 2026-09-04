//! 内核门面 [`Kernel`]：**全部服务提供**的单一入口。
//!
//! 分层（docs/framework-v3.md）：内核 = 所有平台无关的服务逻辑——
//! 数据面（中继 / 推流 / 观看）、信令面（协商 / 控制面 / 引导 / 订阅 /
//! 文件传输）、端点框架（节点 → 设备 → 端点）、会话 / 路由 / 设备图、
//! mDNS 发现、推流引擎与接收编排。壳层（CLI / GUI）只做参数解析、展示与
//! 平台适配（经 [`stross_bridge`] 桥接层注入数据目录 / 主机名 / 平台设备）。
//!
//! * 设备图（[`graph`]）：局域网内节点的能力注册与发现结果聚合
//! * 会话管理（[`session`]）：会话拓扑（source → sinks[]）与协商结果
//! * 路由（[`session::Router`]）：传输方向控制（直连 / 经中继 / 组播）
//! * 鉴权（[`auth`]）：会话级访问码（PIN）策略
//! * 端点（[`endpoint`]）：三层统一注册表（节点 → 端点 → 策略）与订阅联动
//! * 数据面接线（[`data_plane`]）：受控中继作为数据面后端
//!
//! 所有变更通过 [`KernelEvent`] 广播给 UI（替代轮询）。
//! 内核**零路径 / 零 OS 调用 / 零平台分支**（仅有播放能力可用性的
//! `target_os` 分支，属能力交付而非逻辑分支）。
//!
//! `impl Kernel` **按域拆分**到子模块（`anchor` / `streams` / `receive` /
//! `session_api` / `endpoint_api`）——单一门面与公共 API 不变，方法面按职责
//! 分文件提升内聚与可读性；本文件保留门面定义、注入/接线/事件与契约实现。

pub mod auth;
pub mod data_plane;
pub mod endpoint;
pub mod graph;
pub mod id;
pub mod session;

// `impl Kernel` 按域拆分（docs/framework-v3.md：单一门面不变，
// 方法面按职责分文件，提升内聚与可读性）：
pub(crate) mod anchor; // 锚点 / 受控中继 / mDNS 广播 / 发现清单
pub(crate) mod endpoint_api; // 端点框架（通告 / 注册表 / 共享生命周期）
pub(crate) mod receive; // 接收编排（多链路 / 旧 main 槽兼容）
pub(crate) mod session_api; // 会话 / 路由 / 鉴权 / 凭证 / 设备图
pub(crate) mod share_service; // v3 §3.3：impl stross_share::ShareService（共享契约）
pub(crate) mod streams; // 推流引擎 / 采集状态
pub(crate) mod subscribe_service; // v3 §3.4：impl stross_subscribe::SubscribeService（订阅契约）

pub use auth::{AuthError, AuthPolicy, PinAuthPolicy};
pub use data_plane::{DataPlaneBackend, RelayDataPlane};
// 内核 id 新类型（从 stross_view::id 接入）。
pub use id::{Id, LinkId, NodeId, StreamId, StreamKey};
// 端点契约与端点实现（插件区 stross-endpoint）：本模块只保留注册表
// （节点表 NodeEntry + 独立端点表 EndpointRegistry / EndpointEntry / FileSource），
// 路径经 stross_kernel 根部重导出。
pub use endpoint::{
    EndpointEntry, EndpointRegistration, EndpointRegistry, FileSource, NodeEntry, NodeRegistration,
    SubscribeEndpointFactory, UnifiedRegistry,
};
pub use graph::{NodeInfo, NodeRole, TransportAddr};
pub use session::{Negotiated, Session, SessionPrefs};
pub use stross_endpoint::{
    Endpoint, EndpointBase, EndpointClass, FileEndpoint, FileHost, FileReceiveEndpoint, MediaHost,
    MediaReceiveEndpoint, MediaSourceEndpoint, MicEndpoint, Probe, Runtime, ScreenEndpoint,
    ShareEndpoint, ShareHost, StreamHost, SubscribeCtx, SubscribeEndpoint, SubscribeHost,
    SystemAudioEndpoint, TargetKind,
};

// mDNS 实例名派生（单一真源在 anchor 域；测试经 `super::anchor::relay_mdns_instance` 引用）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use stross_endpoint::capture::CaptureBackend;
use stross_endpoint::pipeline::StreamConfig;
#[cfg(not(target_os = "android"))]
use stross_endpoint::playback::AudioOut;
use stross_endpoint::share::file::FilePushOptions;
use stross_proto::message::{EndpointId, EndpointState, ShareToken, SubscribeSpec};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::engine::SenderEngine;
use crate::lock::MutexExt;
use crate::negotiator::NodeIdentity;
use crate::receiver::Receiver;
use crate::relay::{RelayEvent, RelayHandle};
use stross_proto::message::Platform;
use stross_view::{AppInfo, EndpointSourceList};

use self::graph::NodeGraph;
use self::session::SessionManager;

/// 内核事件（推给 UI，替代轮询）——§7.1 类型去重：单一真源在
/// [`stross_view::KernelEvent`]（八概念变体，旧 SessionStarted/Routed/Ended
/// 变体随会话类方法面收敛删除），本模块重导出保持 `stross_kernel::KernelEvent`
/// 路径兼容（壳层 P3 迁移后改直接引用 stross-view）。
pub use stross_view::KernelEvent;

/// 内核门面：控制面（会话 / 路由 / 鉴权 / 凭证）+ 运行态（锚点 / 推流 /
/// 接收 / 端点 / 身份 / 平台标签）全在一处。
///
/// **单一门面不变**（docs/framework-v3.md）：`impl Kernel` 按域拆分到
/// 子模块（`anchor` / `streams` / `receive` / `session_api` / `endpoint_api`），
/// 字段 `pub(crate)` 供同 crate 各域 impl 访问，公共 API 与调用方路径不变。
pub struct Kernel {
    // -- 控制面：设备图 / 会话 / 路由 / 鉴权 / 凭证 --
    pub(crate) graph: NodeGraph,
    pub(crate) sessions: SessionManager,
    pub(crate) auth: Arc<dyn AuthPolicy>,
    pub(crate) next_id: AtomicU64,
    pub(crate) events: broadcast::Sender<KernelEvent>,
    /// 数据面后端（内嵌受控中继等；`None` = 未接线，会话不驱动数据面）。
    pub(crate) data_plane: Mutex<Option<Arc<dyn DataPlaneBackend>>>,
    /// 数据面事件转发任务（[`RelayEvent`] → [`KernelEvent`]）。
    pub(crate) data_plane_task: Mutex<Option<JoinHandle<()>>>,
    /// 接入凭证签发表（`Id` → 签发时的完整凭证；`Arc` 供数据面校验器共享，
    /// 校验器只持本表引用，不形成循环引用）。
    pub(crate) share_tokens: Arc<Mutex<HashMap<Id, ShareToken>>>,
    // -- 运行态：平台标签 / 推流 / 接收 / 端点 / 身份 --
    /// 平台标签（纯值；设备能力枚举等平台知识在 stross-bridge）。
    pub(crate) platform: Platform,
    /// 运行中推流（`Arc` 供数据面事件转发任务共享：流结束时清理引擎状态）。
    /// **并发流**：端点模型的「任意端点可推送/订阅」目标要求一个节点能同时推多路
    /// 流（如屏幕 + 系统声音），故按 `stream_id` 管理多台推流引擎（原来是单引擎）。
    pub(crate) engines: Arc<Mutex<HashMap<Id, RunningStream>>>,
    /// 本机锚点：常驻受控中继 + mDNS 广播（免先连：一起启动、生命周期一致）。
    pub(crate) anchor: Mutex<Option<LocalAnchor>>,
    /// 「可被发现」（mDNS 广播本机）：显式用户开关，默认**关**。
    /// 开启时锚定中继才广播 mDNS、通告端点会即时刷新 TXT 摘要；
    /// 关闭时本机不被局域网 mDNS 扫描发现（设备仍可直连协商）。
    pub(crate) discoverable: AtomicBool,
    /// 采集后端（平台相关，UI 层注入；`Arc` 使其可被引擎复用）。
    pub(crate) backend: Mutex<Option<Arc<dyn CaptureBackend>>>,
    /// 端点框架：**节点表持端点引用 + 独立端点表**（v3 §3.2/§4——本机与互联
    /// 节点同一张节点表，「节点上的端点」= 按 `endpoint_ids` 查端点表的查询
    /// 投影；docs/framework-v3.md §3.2）。
    pub(crate) registry: Mutex<UnifiedRegistry>,
    /// 端点共享登记（stream_id → 共享登记）：实时目标生命周期治理——
    /// watchers=0 自动收尾 / 取消通告联动停止 / 同端点订阅收敛（iteration-plan.md 第十二轮）。
    /// v3 P2d：登记条目统一为契约类型 [`stross_share::ActiveShare`]（§7.1 类型去重——
    /// 契约 crate 为真源，删除本 crate 旧定义）；键为强类型 [`StreamId`]。
    pub(crate) active_shares: Mutex<HashMap<StreamId, stross_share::ActiveShare>>,
    /// watchers 归零后停止端点共享的延迟（给订阅者重连 / 新订阅者接入窗口；测试注入）。
    pub(crate) share_stop_delay: Duration,
    /// 端点共享启动后无任何观看者的接入窗口（订阅者从未接入时兜底停止）。
    pub(crate) share_idle_delay: Duration,
    /// 接收播放：WS 收流 → 抖动缓冲 → 播放/解码（Android 走 raw 帧路径）。
    /// 接收链路注册表：link_id → 接收会话。**多端点链接**（通信模式 v2
    /// Phase C「接收端多流化」）：一次可同时接收多条流（如屏幕 + 系统声音
    /// 同播），每条链独立启停 / 统计，停一条不级联其它链。旧单流 API
    /// （`start_receive` 等）与 Android 播放路径统一落到预留槽 `main`。
    pub(crate) receivers: Mutex<HashMap<Id, Arc<Receiver>>>,
    /// 接收链路元数据（[`SubscribeService::links`] 投影补充）：receivers 表
    /// 只管运行态（stats），节点/端点/流身份经本表记录——契约
    /// [`stross_subscribe::SubscribeService::subscribe`] 与壳层
    /// [`Kernel::start_receive_link`] 登记。
    pub(crate) receive_link_meta: Mutex<HashMap<Id, ReceiveLinkMeta>>,
    /// 契约订阅链路 id 分配器（[`stross_subscribe::SubscribeService::subscribe`]；
    /// 数值单调，`0` = 预留 `main` 槽）。
    pub(crate) next_link_id: AtomicU64,
    /// Arc 自引用（[`Kernel::new_arc`] 经 `Arc::new_cyclic` 登记）：契约实现
    /// （`ShareService::on_subscribed` / `SubscribeService::subscribe`）需要
    /// `Arc<dyn ShareHost>` / `Arc<dyn Runtime>` 能力对象（端点 `share`/
    /// `subscribe` 签名要求），从 `&self` 出发只有经自引用升级才能构造——
    /// 调用方不必逐个传 Arc。
    pub(crate) self_arc: Mutex<Option<std::sync::Weak<Kernel>>>,
    /// 本机持久化身份（`load_or_create_identity` 注入；用于 mDNS 实例名
    /// 唯一化——多设备同端口广播不再同名串扰）。
    pub(crate) identity: Mutex<Option<NodeIdentity>>,
    /// 实例启动时刻（控制面 Status 的 uptime 统计源）。
    pub(crate) started: std::time::Instant,
    /// 节点间对等通道管理器（全双工文字与文件互传）。
    pub channel_manager: Arc<crate::channel::ChannelManager>,
}

/// 本机锚点（免先连：应用打开即自动建立；推流 / 观看 / 局域网发现共用）。
pub(crate) struct LocalAnchor {
    pub(crate) handle: RelayHandle,
    /// mDNS 广播句柄（`None` = 未广播：启动时未开可被发现，或广播失败）。
    /// [`apply_discoverable`] 按开关收敛其生命周期（开启建 / 关闭停），
    /// 不再当作常驻随手持有。
    /// v3 P2b：实现迁至 stross-discovery（`MdnsDiscovery`）。
    pub(crate) discovery: Option<stross_discovery::MdnsDiscovery>,
    /// 中继实际监听端口（绑定 0 自动分配时取实际值）。
    pub(crate) port: u16,
    /// 广播主机名（重注册 / 刷新摘要需要；由锚定流程注入）。
    pub(crate) hostname: String,
}

/// 运行中的推流。
pub(crate) struct RunningStream {
    pub(crate) engine: SenderEngine,
    pub(crate) relay_port: u16,
    pub(crate) title: String,
    pub(crate) stream_id: StreamId,
    pub(crate) started_at: u64,
}

/// 接收链路元数据（[`SubscribeService::links`] 投影补充）：receivers 表只管
/// 运行态，节点/端点/流身份经本表记录。既有壳层链路（`start_receive_link`）
/// 只登记流 id；契约链路（`SubscribeService::subscribe`）登记完整三元组。
#[derive(Debug, Clone, Default)]
pub(crate) struct ReceiveLinkMeta {
    /// 订阅方看到的对端节点（未知 = `NodeId::NIL`）。
    pub(crate) node_id: NodeId,
    /// 订阅目标端点（未知 = `None`；投影时回落占位）。
    pub(crate) endpoint_id: Option<EndpointId>,
    /// 数据面流 id（既有壳层链路登记；未知 = `None`）。
    pub(crate) stream_id: Option<StreamId>,
}

impl Default for Kernel {
    fn default() -> Self {
        Self::new(Platform::Desktop)
    }
}

impl Kernel {
    pub fn new(platform: Platform) -> Self {
        Self::with_auth(platform, Arc::new(PinAuthPolicy::default()))
    }

    /// Arc 自引用构造（生产壳层统一入口）：经 `Arc::new_cyclic` 在构造时登记
    /// 自引用——[`ShareService`](stross_share::ShareService) /
    /// [`SubscribeService`](stross_subscribe::SubscribeService) 契约方法
    /// （`on_subscribed` / `subscribe`）需要 `Arc<dyn ShareHost>` 等能力对象
    /// （端点 `share` / `subscribe` 签名要求），从 `&self` 出发只能经自引用
    /// 升级构造。直接 `Kernel::new` 构造（测试 / 无契约调用场景）自引用为空，
    /// 契约方法在无自引用时显式报错 / 降级（不触发端点回调）。
    pub fn new_arc(platform: Platform) -> Arc<Self> {
        Arc::new_cyclic(|weak| {
            let k = Self::with_auth(platform, Arc::new(PinAuthPolicy::default()));
            *k.self_arc.lock_poisoned() = Some(weak.clone());
            k
        })
    }

    /// 登记 Arc 自引用（`Arc<Kernel>` 到达内核内部的入口处调用一次；幂等——
    /// [`ShareNegotiator::start`] 与 [`Kernel::attach_data_plane`] 已接）。
    pub(crate) fn remember_self(&self, this: &Arc<Self>) {
        *self.self_arc.lock_poisoned() = Some(Arc::downgrade(this));
    }

    /// 升级自引用（契约方法构造 `Arc<dyn ShareHost>` 等能力对象用；非 Arc
    /// 构造返回 `None`）。
    pub(crate) fn self_arc(&self) -> Option<Arc<Self>> {
        self.self_arc
            .lock_poisoned()
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
    }

    /// 注入自定义鉴权策略（远期 WASM 插件等）。
    pub fn with_auth(platform: Platform, auth: Arc<dyn AuthPolicy>) -> Self {
        let (events, _rx) = broadcast::channel(64);
        Self {
            graph: NodeGraph::default(),
            sessions: SessionManager::default(),
            auth,
            next_id: AtomicU64::new(1),
            events,
            data_plane: Mutex::new(None),
            data_plane_task: Mutex::new(None),
            share_tokens: Arc::new(Mutex::new(HashMap::new())),
            platform,
            engines: Arc::new(Mutex::new(Default::default())),
            anchor: Mutex::new(None),
            discoverable: AtomicBool::new(false),
            backend: Mutex::new(None),
            registry: Mutex::new(UnifiedRegistry::new()),
            active_shares: Mutex::new(HashMap::new()),
            share_stop_delay: Duration::from_secs(4),
            share_idle_delay: Duration::from_secs(10),
            receivers: Mutex::new(HashMap::new()),
            receive_link_meta: Mutex::new(HashMap::new()),
            next_link_id: AtomicU64::new(0),
            self_arc: Mutex::new(None),
            identity: Mutex::new(None),
            started: std::time::Instant::now(),
            channel_manager: Arc::new(crate::channel::ChannelManager::new(
                std::env::temp_dir().join("stross-downloads"),
                true,
            )),
        }
    }

    // -----------------------------------------------------------------------
    // 平台标签 / 注入
    // -----------------------------------------------------------------------

    /// 运行平台标签（"desktop" / "android"；控制面 Status 与 app_info 展示）。
    pub const fn platform(&self) -> Platform {
        self.platform
    }

    /// 运行平台字符串。
    pub const fn platform_str(&self) -> &'static str {
        self.platform.as_str()
    }

    /// 注入采集后端（UI 层在启动时调用一次）。
    pub fn set_backend(&self, backend: Arc<dyn CaptureBackend>) {
        *self.backend.lock_poisoned() = Some(backend);
    }

    /// 登记一个端点并立即 load（探测可挂载性；幂等：按端点 id 去重）。
    ///
    /// 平台端点构造（探测闭包注入）由桥接层提供；load 失败不阻止登记——
    /// 端点保留但标记不可挂载（`available=false` + `last_error`）。
    pub fn seed_endpoint(&self, ep: Box<dyn ShareEndpoint>) {
        self.registry.lock_poisoned().seed(ep);
    }

    /// 注入本机持久化身份（UI 层启动时调用；缺失时 mDNS 实例名回退旧格式）。
    /// 同时把本机登记为统一注册表的自节点（`(节点, 端点, 策略)` 查表的
    /// 本机分支键，docs/framework-v3.md §2）。
    pub fn set_identity(&self, id: NodeIdentity) {
        self.registry
            .lock_poisoned()
            .set_self_node(id.node_id, &id.node_name);
        *self.identity.lock_poisoned() = Some(id);
    }

    /// 本机持久化身份（目录 API 的 node 信息源）。
    pub fn node_identity(&self) -> Option<NodeIdentity> {
        self.identity.lock_poisoned().clone()
    }

    // -----------------------------------------------------------------------
    // 信息与设备
    // -----------------------------------------------------------------------

    /// 应用信息（版本 / 平台 / ffmpeg 是否可用 / 本机 IP）。
    pub fn app_info(&self) -> AppInfo {
        AppInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            platform: self.platform.as_str().to_string(),
            ffmpeg: stross_endpoint::pipeline::ffmpeg_available(),
            ips: crate::net::local_ips()
                .into_iter()
                .map(|ip| ip.to_string())
                .collect(),
            node_id: self
                .node_identity()
                .map(|i| i.node_id)
                .unwrap_or(NodeId::NIL),
        }
    }

    /// 摄像头 / 麦克风 / 系统声音端点源列表。
    pub fn list_endpoint_sources(&self) -> EndpointSourceList {
        EndpointSourceList {
            cameras: stross_endpoint::sources::list_cameras(),
            audio_inputs: stross_endpoint::sources::list_audio_inputs(),
            system_audio: stross_endpoint::sources::list_system_audio(),
        }
    }

    // -----------------------------------------------------------------------
    // 数据面接线
    // -----------------------------------------------------------------------

    /// 接入数据面后端（内嵌受控中继）：订阅其流生命周期事件并转发为
    /// [`KernelEvent`]（StreamStarted / StreamEnded / WatchersChanged）；
    /// 同时注入接入凭证校验器（B 阶段跨设备推流：受控中继在预授权之外
    /// 接受本内核签发的 [`ShareToken`]）。
    ///
    /// 事件转发同时承担**端点共享生命周期治理**（iteration-plan.md 第十二轮）：
    /// * `StreamEnded` → 清共享登记 + 复位状态 + 本机会话 teardown（会话生命周期 = 流生命周期）；
    /// * `WatchersChanged{0}` → 延迟复查后停止端点共享（订阅者全部断开自动收尾）。
    pub fn attach_data_plane(self: &Arc<Self>, backend: Arc<dyn DataPlaneBackend>) {
        // 登记 Arc 自引用（契约实现需要 `Arc<dyn ShareHost>` 能力对象；幂等）
        self.remember_self(self);
        backend.set_share_token_validator(self.token_validator());
        let mut rx = backend.events();
        *self.data_plane.lock_poisoned() = Some(backend);
        let events = self.events.clone();
        let me = self.clone();
        let stop_delay = self.share_stop_delay;
        let task = tokio::spawn(async move {
            while let Ok(ev) = rx.recv().await {
                let kernel_ev = match ev {
                    RelayEvent::StreamStarted { stream_id, .. } => KernelEvent::StreamStarted {
                        session_id: stream_id,
                    },
                    RelayEvent::StreamEnded { stream_id } => {
                        // 数据面流结束 → 统一收尾（清登记 + 复位状态 + 停引擎 +
                        // 拆会话；推流端断开 / 静默超时 / 显式停止 / revoke 均触发）。
                        // 与 [`Kernel::stop_share_by_stream`] 共用同一清理逻辑（单一真源）。
                        me.reap_stream(&Id::from(stream_id.as_str()));
                        KernelEvent::StreamEnded {
                            session_id: stream_id,
                        }
                    }
                    RelayEvent::WatchersChanged {
                        stream_id,
                        watchers,
                    } => {
                        // 端点共享自动收尾：watchers 归零 → 延迟复查仍无人观看才停
                        // （给订阅者重连 / 新订阅者接入窗口）
                        if watchers == 0
                            && me
                                .active_share_by_stream(&Id::from(stream_id.as_str()))
                                .is_some()
                        {
                            let me2 = me.clone();
                            let sid = Id::from(stream_id.as_str());
                            let delay = stop_delay;
                            tokio::spawn(async move {
                                tokio::time::sleep(delay).await;
                                // P2e：watchers 归零复查经契约 `ShareService::reap_if_unwatched`
                                // （自有 stop_share_if_unwatched 已删除，方法体收敛进契约）。
                                stross_share::ShareService::reap_if_unwatched(&*me2, &sid);
                            });
                        }
                        // 订阅数同步：端点共享的流 subscribers = watchers
                        if let Some(share) =
                            me.active_share_by_stream(&Id::from(stream_id.as_str()))
                        {
                            me.registry.lock_poisoned().set_state(
                                share.endpoint_id,
                                EndpointState::Active,
                                watchers,
                            );
                        }
                        KernelEvent::WatchersChanged {
                            session_id: stream_id,
                            watchers,
                        }
                    }
                };
                let _ = events.send(kernel_ev);
            }
        });
        // 替换旧的数据面转发任务（若存在）：防重复 attach 时旧任务滞留
        // （旧后端事件流关闭即退出，但显式 abort 更即时）；abort 的 JoinHandle
        // 不再更新，避免进程生命周期内残留孤儿协程。
        if let Some(old) = self.data_plane_task.lock_poisoned().take() {
            old.abort();
        }
        *self.data_plane_task.lock_poisoned() = Some(task);
    }

    /// 是否已接入数据面。
    pub fn has_data_plane(&self) -> bool {
        self.data_plane.lock_poisoned().is_some()
    }

    /// 订阅内核事件。
    pub fn subscribe(&self) -> broadcast::Receiver<KernelEvent> {
        self.events.subscribe()
    }

    /// 实例已运行秒数（控制面 Status 展示 uptime）。
    pub fn uptime_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }
}

/// 端点注入目标（stross-endpoint 端点装配用）：登记 + 平台查询。
impl stross_endpoint::factory::EndpointSeeder for Kernel {
    fn seed_endpoint(&self, ep: Box<dyn ShareEndpoint>) -> bool {
        self.seed_endpoint(ep);
        true
    }
    fn platform(&self) -> Platform {
        Self::platform(self)
    }
}

/// 端点层可见的内核调度能力（v3 §3.2 四能力契约实现，取代旧聚合
/// `EndpointApp`）：推流 / 中继端口 / 文件泵 / 媒体接收 / 运行时——
/// 内核 = 纯管理调度，数据面执行在端点层；生命周期治理
/// （note_share_active / stop_share_if_unwatched）从契约删除，
/// 内核保留为自有方法（P2e 迁 `stross-share::ShareService`）。
#[async_trait::async_trait]
impl stross_endpoint::contract::StreamHost for Kernel {
    async fn start_stream(
        &self,
        cfg: StreamConfig,
        relay_url: Option<String>,
    ) -> anyhow::Result<stross_view::StartResult> {
        Self::start_stream(self, cfg, relay_url)
            .await
            .map_err(anyhow::Error::msg)
    }

    fn relay_port(&self) -> Option<u16> {
        Self::relay_port(self)
    }
}

#[async_trait::async_trait]
impl stross_endpoint::contract::FileHost for Kernel {
    async fn push_file(&self, path: PathBuf, opts: FilePushOptions) -> anyhow::Result<u64> {
        crate::file_xfer::push_file(&path, &opts).await
    }

    async fn receive_file(
        &self,
        watch_url: String,
        stream_id: StreamId,
        out_dir: PathBuf,
    ) -> anyhow::Result<stross_view::ReceivedFile> {
        // 订阅端点生成路径共享此竞态收敛）
        crate::subscriber::receive_file_retry(&watch_url, &stream_id, &out_dir).await
    }
}

#[async_trait::async_trait]
impl stross_endpoint::contract::MediaHost for Kernel {
    /// 媒体接收（订阅端 Graph/Audio 类执行，播放器入端点）：按订阅规格的
    /// pick 规则解读 + 解码，阻塞到流结束返回解码帧数。
    ///
    /// 桌面走 `Receiver`（ffmpeg 解码，音频丢弃——自治接收语义；GUI 播放
    /// 路径仍走 `start_receive` 命令）；Android 播放由壳层 `start_receive_raw`
    /// 承担（Kotlin MediaCodec），本路径暂不支持（返回明确错误）。
    #[cfg(not(target_os = "android"))]
    async fn receive_media(&self, spec: &SubscribeSpec) -> anyhow::Result<u64> {
        // 序列化 = 内核数据契约：订阅端按策略装载解读前先校验内核序列化工具
        // 支持（未实现规则拒绝，不静默降级）
        if stross_serialize::loader_for(&spec.strategy).is_none() {
            return Err(anyhow::anyhow!(
                "内核不支持序列化规则 {:?}（数据契约不匹配，订阅拒绝）",
                spec.strategy.serialize
            ));
        }
        let relay_url = spec
            .relay_url
            .clone()
            .ok_or_else(|| anyhow::anyhow!("媒体订阅端点缺公开方中继地址（pull 未锚定）"))?;
        let recv = Receiver::start_with_rule(
            relay_url,
            spec.stream_id.to_string(),
            AudioOut::Discard,
            None,
            spec.strategy.pick,
        )
        .await
        .map_err(|e| anyhow::anyhow!("媒体订阅端点接收启动失败: {e}"))?;
        let mut frames = recv
            .take_frames()
            .ok_or_else(|| anyhow::anyhow!("媒体订阅端点接收通道未就绪"))?;
        let mut count = 0u64;
        while frames.recv().await.is_some() {
            count += 1;
        }
        Ok(count)
    }
    #[cfg(target_os = "android")]
    async fn receive_media(&self, _spec: &SubscribeSpec) -> anyhow::Result<u64> {
        Err(anyhow::anyhow!(
            "Android 媒体订阅端点暂由壳层 start_receive_raw 承担（播放器入端点为桌面路径，后续按族接入）"
        ))
    }
}

/// 端点自驱动辅助：内核在运行时上下文 spawn 异步任务（端点 `share`/
/// `subscribe` 的 fire-and-forget 载体——契约层零 tokio 依赖）。
impl stross_endpoint::contract::Runtime for Kernel {
    fn spawn_task(&self, fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>) {
        tokio::spawn(fut);
    }
}

// `ShareHost`（= StreamHost + FileHost）与 `SubscribeHost`（= MediaHost + FileHost）
// 由 stross-endpoint 契约侧的 blanket impl 自动覆盖（内核已实现全部四个能力
// trait），无需（也不可）显式书写——分享端 `share` 经 `Arc<dyn ShareHost>`、
// 订阅端 `subscribe` 经 `Arc<dyn SubscribeHost>` 各取所需。

/// 当前 Unix 秒（公共实现见 [`stross_proto::time`]；session_api 等域 impl 共用）。
pub(crate) fn now_secs() -> u64 {
    stross_proto::time::unix_secs()
}

/// 一次性凭证 PIN（6 位数字）。
///
/// 非密码学随机（一次性凭证防误连/旁观冒用即可）：`fastrand` 全局 PRNG 由
/// OS 熵种子初始化，无需自建 Hasher 混种，比手写 `DefaultHasher` 更不可预测
/// 且更简洁。
pub(crate) fn random_pin(_seed: &str) -> String {
    format!("{:06}", fastrand::u32(0..1_000_000))
}

#[cfg(test)]
mod kernel_tests;
