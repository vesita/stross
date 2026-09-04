//! 内核门面 [`Kernel`]：**全部服务提供**的单一入口。
//!
//! 分层（docs/layering-architecture.md）：内核 = 所有平台无关的服务逻辑——
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

// `impl Kernel` 按域拆分（docs/layering-architecture.md：单一门面不变，
// 方法面按职责分文件，提升内聚与可读性）：
pub(crate) mod anchor; // 锚点 / 受控中继 / mDNS 广播 / 发现清单
pub(crate) mod endpoint_api; // 端点框架（通告 / 注册表 / 共享生命周期）
pub(crate) mod receive; // 接收编排（多链路 / 旧 main 槽兼容）
pub(crate) mod session_api; // 会话 / 路由 / 鉴权 / 凭证 / 设备图
pub(crate) mod streams; // 推流引擎 / 采集状态

pub use auth::{AuthError, AuthPolicy, PinAuthPolicy};
pub use data_plane::{DataPlaneBackend, RelayDataPlane};
// 内核 id 新类型（内部业务 id 用；壳层仍传 &str，见 id.rs 边界约定）。
pub use id::Id;
// 端点契约与端点实现（插件区 stross-endpoint）：本模块只保留注册表
// （EndpointRegistry / EndpointEntry / FileSource），路径经 stross_kernel 根部重导出。
pub use endpoint::{
    EndpointEntry, EndpointRegistration, EndpointRegistry, FileSource, NodeRegistration,
    UnifiedRegistry,
};
pub use graph::{NodeInfo, NodeRole, TransportAddr};
pub use session::{Negotiated, Session, SessionPrefs};
pub use stross_endpoint::{
    Endpoint, EndpointApp, EndpointBase, EndpointClass, FileEndpoint, FileReceiveEndpoint,
    MediaReceiveEndpoint, MediaSourceEndpoint, MicEndpoint, Probe, ScreenEndpoint, ShareEndpoint,
    SubscribeCtx, SubscribeEndpoint, SystemAudioEndpoint, TargetKind,
};

// mDNS 实例名派生（单一真源在 anchor 域；测试经 `super::anchor::relay_mdns_instance` 引用）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;

use stross_endpoint::capture::CaptureBackend;
use stross_endpoint::pipeline::StreamConfig;
#[cfg(not(target_os = "android"))]
use stross_endpoint::playback::AudioOut;
use stross_endpoint::share::file::FilePushOptions;
use stross_proto::message::{
    Delivery, EndpointId, EndpointState, RoutePath, ShareToken, StreamInfo, SubscribeSpec,
};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::engine::SenderEngine;
use crate::lock::MutexExt;
use crate::negotiator::DeviceIdentity;
use crate::receiver::Receiver;
use crate::relay::{RelayEvent, RelayHandle};
use stross_proto::message::Platform;
use stross_types::{AppInfo, DeviceList};

use self::graph::DeviceGraph;
use self::session::SessionManager;

/// 内核事件（推给 UI，替代轮询）。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum KernelEvent {
    SessionStarted {
        session: Session,
    },
    SessionRouted {
        session_id: String,
        path: RoutePath,
    },
    SessionEnded {
        session_id: String,
    },
    /// 数据面流启动（内嵌中继上报；D4：session_id 与 stream_id 合一）。
    StreamStarted {
        session_id: String,
        info: StreamInfo,
    },
    /// 数据面流结束。
    StreamEnded {
        session_id: String,
    },
    /// 观看者数量变化。
    WatchersChanged {
        session_id: String,
        watchers: u32,
    },
}

/// 内核门面：控制面（会话 / 路由 / 鉴权 / 凭证）+ 运行态（锚点 / 推流 /
/// 接收 / 端点 / 身份 / 平台标签）全在一处。
///
/// **单一门面不变**（docs/layering-architecture.md）：`impl Kernel` 按域拆分到
/// 子模块（`anchor` / `streams` / `receive` / `session_api` / `endpoint_api`），
/// 字段 `pub(crate)` 供同 crate 各域 impl 访问，公共 API 与调用方路径不变。
pub struct Kernel {
    // -- 控制面：设备图 / 会话 / 路由 / 鉴权 / 凭证 --
    pub(crate) graph: DeviceGraph,
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
    /// 端点框架：**三层统一注册表**（节点 → 端点 → 策略；本机 + 互联节点
    /// 同一张表，docs/endpoint-model-v2.md §2）。
    pub(crate) registry: Mutex<UnifiedRegistry>,
    /// 端点共享登记（stream_id → 端点）：实时目标生命周期治理——
    /// watchers=0 自动收尾 / 取消通告联动停止 / 同端点订阅收敛（iteration-plan.md 第十二轮）。
    pub(crate) active_shares: Mutex<HashMap<Id, ActiveShare>>,
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
    /// 本机持久化身份（`load_or_create_identity` 注入；用于 mDNS 实例名
    /// 唯一化——多设备同端口广播不再同名串扰）。
    pub(crate) identity: Mutex<Option<DeviceIdentity>>,
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
    pub(crate) discovery: Option<crate::discovery::Discovery>,
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
    pub(crate) stream_id: String,
    pub(crate) started_at: u64,
}

/// 端点共享登记条目（实时目标；文件端点不登记——有完成态，StreamEnded 统一清理）。
#[derive(Debug, Clone)]
pub(crate) struct ActiveShare {
    pub(crate) endpoint_id: EndpointId,
    pub(crate) delivery: Delivery,
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

    /// 注入自定义鉴权策略（远期 WASM 插件等）。
    pub fn with_auth(platform: Platform, auth: Arc<dyn AuthPolicy>) -> Self {
        let (events, _rx) = broadcast::channel(64);
        Self {
            graph: DeviceGraph::default(),
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
    /// 本机分支键，docs/endpoint-model-v2.md §2）。
    pub fn set_identity(&self, id: DeviceIdentity) {
        self.registry
            .lock_poisoned()
            .set_self_node(&id.device_id, &id.device_name);
        *self.identity.lock_poisoned() = Some(id);
    }

    /// 本机持久化身份（目录 API 的 node 信息源）。
    pub fn device_identity(&self) -> Option<DeviceIdentity> {
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
        }
    }

    /// 摄像头 / 麦克风 / 系统声音设备列表。
    pub fn list_devices(&self) -> DeviceList {
        DeviceList {
            cameras: stross_endpoint::devices::list_cameras(),
            audio_inputs: stross_endpoint::devices::list_audio_inputs(),
            system_audio: stross_endpoint::devices::list_system_audio(),
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
        backend.set_share_token_validator(self.token_validator());
        let mut rx = backend.events();
        *self.data_plane.lock_poisoned() = Some(backend);
        let events = self.events.clone();
        let me = self.clone();
        let stop_delay = self.share_stop_delay;
        let task = tokio::spawn(async move {
            while let Ok(ev) = rx.recv().await {
                let kernel_ev = match ev {
                    RelayEvent::StreamStarted { stream_id, info } => KernelEvent::StreamStarted {
                        session_id: stream_id,
                        info,
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
                                me2.stop_share_if_unwatched(&sid);
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

/// 端点层可见的内核调度能力（端点契约实现）：
/// 推流 / 中继端口 / 文件泵 / 共享登记——内核 = 纯管理调度，数据面执行在端点层。
#[async_trait::async_trait]
impl EndpointApp for Kernel {
    async fn start_stream(
        &self,
        cfg: StreamConfig,
        relay_url: Option<String>,
    ) -> anyhow::Result<stross_types::StartResult> {
        Self::start_stream(self, cfg, relay_url)
            .await
            .map_err(anyhow::Error::msg)
    }

    fn relay_port(&self) -> Option<u16> {
        Self::relay_port(self)
    }

    async fn push_file(&self, path: PathBuf, opts: FilePushOptions) -> anyhow::Result<u64> {
        crate::file_xfer::push_file(&path, &opts).await
    }

    async fn receive_file(
        &self,
        watch_url: String,
        stream_id: String,
        out_dir: PathBuf,
    ) -> anyhow::Result<stross_types::ReceivedFile> {
        // 对「流尚未出现」重试（与 CLI subscribe_file 同语义兜底；
        // 订阅端点生成路径共享此竞态收敛）
        crate::subscriber::receive_file_retry(&watch_url, &stream_id, &out_dir).await
    }

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
        if crate::pick::loader_for(&spec.strategy).is_none() {
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
            spec.stream_id.clone(),
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

    /// 端点自驱动辅助：内核在运行时上下文 spawn 异步任务（端点 `share`/
    /// `subscribe` 的 fire-and-forget 载体——契约层零 tokio 依赖）。
    fn spawn_task(&self, fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>) {
        tokio::spawn(fut);
    }

    fn note_share_active(
        &self,
        self_weak: std::sync::Weak<dyn EndpointApp>,
        endpoint_id: EndpointId,
        stream_id: &str,
        delivery: Delivery,
    ) {
        Self::note_share_active(self, self_weak, endpoint_id, stream_id, delivery);
    }

    fn stop_share_if_unwatched(&self, stream_id: &str) {
        Self::stop_share_if_unwatched(self, &Id::from(stream_id));
    }
}

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
mod tests {
    use super::anchor::relay_mdns_instance;
    use super::*;
    use futures_util::FutureExt;
    use stross_proto::frame::Frame;
    use stross_proto::message::{
        CapabilityDescriptor, CodecId, MediaKind, ReliabilityProfile, TransportId,
    };
    use tokio::sync::mpsc;

    fn node(id: &str) -> NodeInfo {
        NodeInfo {
            node_id: id.into(),
            name: id.into(),
            roles: vec![NodeRole::Sender],
            caps: vec![],
            addrs: vec![],
        }
    }

    /// 测试用假后端：记录是否被调用。
    struct MockBackend(std::sync::atomic::AtomicBool);
    #[async_trait::async_trait]
    impl CaptureBackend for MockBackend {
        async fn start(&self, _cfg: &StreamConfig, _tx: mpsc::Sender<Frame>) -> anyhow::Result<()> {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        fn stop(&self) {
            self.0.store(false, std::sync::atomic::Ordering::SeqCst);
        }
        fn status(&self) -> stross_endpoint::capture::CaptureStatus {
            stross_endpoint::capture::CaptureStatus {
                started: self.0.load(std::sync::atomic::Ordering::SeqCst),
                error: None,
            }
        }
    }

    #[test]
    fn app_info_and_devices_never_panic() {
        let kernel = Kernel::new(Platform::Desktop);
        let info = kernel.app_info();
        assert_eq!(info.platform, "desktop");
        let _ = kernel.list_devices();
    }

    #[test]
    fn capture_status_requires_backend() {
        let kernel = Kernel::new(Platform::Desktop);
        // 未注入后端时采集状态应为未激活
        let st = kernel.capture_status();
        assert!(!st.active);
        assert!(!st.started);
    }

    #[test]
    fn set_backend_then_query() {
        let kernel = Kernel::new(Platform::Android);
        kernel.set_backend(Arc::new(MockBackend(std::sync::atomic::AtomicBool::new(
            false,
        ))));
        let st = kernel.capture_status();
        assert!(!st.active); // 未推流
    }

    #[test]
    fn graph_upsert_and_capability() {
        let k = Kernel::new(Platform::Desktop);
        k.upsert_node(node("a"));
        k.upsert_node(node("b"));
        assert_eq!(k.nodes().len(), 2);
        k.register_capability("a", CapabilityDescriptor::unknown());
        k.register_capability("a", CapabilityDescriptor::unknown()); // 去重
        let a = k.nodes().into_iter().find(|n| n.node_id == "a").unwrap();
        assert_eq!(a.caps.len(), 1);
    }

    #[tokio::test]
    async fn create_session_requires_sinks() {
        let k = Kernel::new(Platform::Desktop);
        assert!(
            k.create_session("a", &[], &SessionPrefs::default())
                .is_err()
        );
    }

    /// 显式 id 会话幂等（docs/comm-mode-v2.md §6「配套改动」）：
    /// 语义 id 派生路径重复建会话返回既有会话，不重复登记。
    #[tokio::test]
    async fn ensure_session_with_id_is_idempotent() {
        let k = Kernel::new(Platform::Desktop);
        let prefs = SessionPrefs::default();
        let s1 = k
            .ensure_session_with_id("ep-screen-ly-rt-x", "local", &["local".into()], &prefs)
            .unwrap();
        assert_eq!(s1.id, "ep-screen-ly-rt-x");
        // 同 id 二次调用：返回既有会话，不新增
        let s2 = k
            .ensure_session_with_id("ep-screen-ly-rt-x", "local", &["local".into()], &prefs)
            .unwrap();
        assert_eq!(s2.id, s1.id);
        assert_eq!(k.sessions().len(), 1, "幂等：同 id 不重复建会话");
        // 不同派生 id → 各自会话（不同端点互不干扰）
        let s3 = k
            .ensure_session_with_id("ep-mic-ly-rt-y", "local", &["local".into()], &prefs)
            .unwrap();
        assert_ne!(s3.id, s1.id);
        assert_eq!(k.sessions().len(), 2);
    }

    #[tokio::test]
    async fn session_lifecycle_events() {
        let k = Kernel::new(Platform::Desktop);
        let mut rx = k.subscribe();

        let s = k
            .create_session("a", &["b".into()], &SessionPrefs::default())
            .unwrap();
        assert_eq!(s.path, RoutePath::Direct { node: "b".into() });
        match rx.recv().now_or_never().unwrap().unwrap() {
            KernelEvent::SessionStarted { session } => assert_eq!(session.id, s.id),
            other => panic!("期望 SessionStarted，得到 {other:?}"),
        }

        // 多接收端 → 组播（会再发一个 SessionStarted，先消费掉）
        let m = k
            .create_session("a", &["b".into(), "c".into()], &SessionPrefs::default())
            .unwrap();
        assert!(matches!(m.path, RoutePath::Mesh { .. }));
        match rx.recv().now_or_never().unwrap().unwrap() {
            KernelEvent::SessionStarted { session } => assert_eq!(session.id, m.id),
            other => panic!("期望 SessionStarted，得到 {other:?}"),
        }

        // 改道
        k.route(
            &s.id,
            RoutePath::ViaRelay {
                node: "relay-1".into(),
            },
        )
        .unwrap();
        assert_eq!(
            k.session(&s.id).unwrap().path,
            RoutePath::ViaRelay {
                node: "relay-1".into()
            }
        );
        match rx.recv().now_or_never().unwrap().unwrap() {
            KernelEvent::SessionRouted { session_id, .. } => assert_eq!(session_id, s.id),
            other => panic!("期望 SessionRouted，得到 {other:?}"),
        }

        // 拆除
        k.teardown(&s.id).unwrap();
        assert!(k.session(&s.id).is_none());
        match rx.recv().now_or_never().unwrap().unwrap() {
            KernelEvent::SessionEnded { session_id } => assert_eq!(session_id, s.id),
            other => panic!("期望 SessionEnded，得到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn route_unknown_session_fails() {
        let k = Kernel::new(Platform::Desktop);
        assert!(
            k.route("nope", RoutePath::Direct { node: "b".into() })
                .is_err()
        );
        assert!(k.teardown("nope").is_err());
    }

    #[tokio::test]
    async fn negotiate_picks_transport_and_codec() {
        use stross_proto::message::CapabilityKind;
        let k = Kernel::new(Platform::Desktop);
        k.upsert_node(NodeInfo {
            node_id: "a".into(),
            name: "a".into(),
            roles: vec![NodeRole::Sender],
            caps: vec![CapabilityDescriptor {
                kind: CapabilityKind::Source,
                media: vec![MediaKind::Screen],
                codecs: vec![CodecId::H264, CodecId::Aac],
                transports: vec![TransportId::Ws],
                max_width: Some(1920),
                max_height: Some(1080),
                preferred_profile: ReliabilityProfile::Lossy,
            }],
            addrs: vec![],
        });
        // 源只支持 ws → 协商出 ws + h264
        let s = k
            .create_session("a", &["b".into()], &SessionPrefs::default())
            .unwrap();
        assert_eq!(s.negotiated.transport, TransportId::Ws);
        assert_eq!(s.negotiated.codec, CodecId::H264);
        // 显式偏好 webrtc 但源不支持 → 回退 ws
        let prefs = SessionPrefs {
            profile: ReliabilityProfile::Lossy,
            preferred_transport: Some(TransportId::WebRtc),
            access_code: None,
            title: String::new(),
        };
        let s2 = k.create_session("a", &["b".into()], &prefs).unwrap();
        assert_eq!(s2.negotiated.transport, TransportId::Ws);
    }

    #[tokio::test]
    async fn pin_gates_control_operations() {
        use stross_proto::message::RoutePath;
        let k = Kernel::new(Platform::Desktop);
        // 设置访问码创建会话
        let prefs = SessionPrefs {
            profile: ReliabilityProfile::Lossy,
            preferred_transport: None,
            access_code: Some("1234".into()),
            title: String::new(),
        };
        let s = k.create_session("a", &["b".into()], &prefs).unwrap();
        assert!(s.requires_pin);
        // 未授权：route / teardown 都被拒绝
        assert!(
            k.route(&s.id, RoutePath::ViaRelay { node: "r".into() })
                .is_err(),
            "未授权 route 应被拒绝"
        );
        assert!(k.teardown(&s.id).is_err(), "未授权 teardown 应被拒绝");
        // 错误访问码
        assert!(k.authorize(&s.id, Some("9999")).is_err());
        assert!(
            k.route(&s.id, RoutePath::ViaRelay { node: "r".into() })
                .is_err()
        );
        // 正确访问码 → 放行
        assert!(k.authorize(&s.id, Some("1234")).is_ok());
        assert!(
            k.route(&s.id, RoutePath::ViaRelay { node: "r".into() })
                .is_ok()
        );
        assert!(k.teardown(&s.id).is_ok());
        // 会话不存在
        assert!(k.authorize("nope", Some("1234")).is_err());
    }

    #[tokio::test]
    async fn force_teardown_cleans_pin_session_without_auth() {
        let k = Kernel::new(Platform::Desktop);
        let prefs = SessionPrefs {
            profile: ReliabilityProfile::Lossy,
            preferred_transport: None,
            access_code: Some("8888".into()),
            title: "受保护会话".into(),
        };
        let s = k.create_session("a", &["b".into()], &prefs).unwrap();
        assert!(s.requires_pin);
        // 普通 teardown 因未授权被拒绝
        assert!(k.teardown(&s.id).is_err());
        assert!(k.session(&s.id).is_some());
        // 内部生命周期 force_teardown 无阻碍彻底清理会话
        assert!(k.force_teardown(&s.id).is_ok());
        assert!(k.session(&s.id).is_none());
    }

    #[tokio::test]
    async fn no_pin_session_stays_open() {
        let k = Kernel::new(Platform::Desktop);
        let s = k
            .create_session("a", &["b".into()], &SessionPrefs::default())
            .unwrap();
        assert!(!s.requires_pin);
        assert!(
            k.route(&s.id, RoutePath::ViaRelay { node: "r".into() })
                .is_ok(),
            "无访问码会话应直接放行"
        );
    }

    #[tokio::test]
    async fn share_token_lifecycle() {
        let k = Kernel::new(Platform::Desktop);
        let s = k
            .create_session("a", &["b".into()], &SessionPrefs::default())
            .unwrap();

        // 未知会话 → 拒绝
        assert!(
            k.create_share_token("nope", vec![MediaKind::Mic], Duration::from_secs(60))
                .is_err()
        );

        // 签发：stream_id 与会话一致、PIN 为 6 位数字、有效期正确
        let token = k
            .create_share_token(&s.id, vec![MediaKind::Mic], Duration::from_secs(60))
            .unwrap();
        assert_eq!(token.stream_id, s.id);
        assert_eq!(token.v, ShareToken::VERSION);
        assert!(token.pin.len() == 6 && token.pin.chars().all(|c| c.is_ascii_digit()));
        assert_eq!(token.expires_at, now_secs().saturating_add(60));

        // 校验通过
        assert!(k.verify_share_token(&token).is_ok());

        // 篡改 PIN → 拒绝（逐字比对）
        let mut forged = token.clone();
        forged.pin = "000000".into();
        assert!(k.verify_share_token(&forged).is_err());

        // 篡改 stream_id → 拒绝（查不到签发记录）
        let mut forged2 = token.clone();
        forged2.stream_id = "sess-other".into();
        assert!(k.verify_share_token(&forged2).is_err());

        // 重新签发覆盖旧凭证（同会话最新凭证有效）
        let token2 = k
            .create_share_token(&s.id, vec![MediaKind::Mic], Duration::from_secs(60))
            .unwrap();
        assert!(k.verify_share_token(&token2).is_ok());
        assert!(k.verify_share_token(&token).is_err(), "旧凭证应失效");

        // ttl=0 → 立即过期
        let expired = k
            .create_share_token(&s.id, vec![MediaKind::Mic], Duration::ZERO)
            .unwrap();
        assert!(k.verify_share_token(&expired).is_err());
    }

    #[test]
    fn relay_mdns_instance_unique_per_device_same_port() {
        // 不同 device_id、同端口：实例名必须不同（mdns-sd 同名互覆盖的根因）
        let a = relay_mdns_instance(Some("0123456789abcdef0123456789abcdef"), 8777);
        let b = relay_mdns_instance(Some("fedcba9876543210fedcba9876543210"), 8777);
        assert_ne!(a, b, "同端口不同设备实例名必须不同");
        assert!(
            a.starts_with("stross-01234567-8777"),
            "实例名携带设备前缀: {a}"
        );
    }

    #[test]
    fn relay_mdns_instance_same_device_stable() {
        // 同一设备（同 device_id）跨启动实例名稳定（端口不变时）
        let id = "deadbeefcafe0123deadbeefcafe0123";
        assert_eq!(
            relay_mdns_instance(Some(id), 8777),
            relay_mdns_instance(Some(id), 8777)
        );
        // 端口变化只影响后缀（设备身份恒在前缀）
        assert_ne!(
            relay_mdns_instance(Some(id), 8777),
            relay_mdns_instance(Some(id), 33462)
        );
    }

    #[test]
    fn relay_mdns_instance_fallback_without_identity() {
        // 未注入身份：回退旧格式（兼容无 UI 接入方）
        assert_eq!(relay_mdns_instance(None, 8777), "sender-8777");
        assert_eq!(relay_mdns_instance(Some(""), 8777), "sender-8777");
    }

    /// 端点共享登记 → 查询 → 显式停止（stop_endpoint_share）→ 登记清除。
    #[tokio::test]
    async fn active_share_register_query_and_stop() {
        let k = Arc::new(Kernel::new(Platform::Desktop));
        let weak: std::sync::Weak<dyn EndpointApp> =
            Arc::downgrade(&(k.clone() as Arc<dyn EndpointApp>));
        k.note_share_active(
            weak,
            EndpointId::new(MediaKind::Mic, 0),
            "sess-1",
            Delivery::Pull,
        );
        let got = k
            .active_share_by_endpoint(EndpointId::new(MediaKind::Mic, 0))
            .expect("登记后可查询");
        assert_eq!(got.0, "sess-1");
        assert_eq!(got.1, Delivery::Pull);

        k.stop_endpoint_share(EndpointId::new(MediaKind::Mic, 0))
            .unwrap();
        assert!(
            k.active_share_by_endpoint(EndpointId::new(MediaKind::Mic, 0))
                .is_none(),
            "停止后登记应清除"
        );
        // 幂等：无活动共享时停止直接成功
        assert!(
            k.stop_endpoint_share(EndpointId::new(MediaKind::Mic, 0))
                .is_ok()
        );
        assert!(
            k.stop_endpoint_share(EndpointId::new(MediaKind::Screen, 0))
                .is_ok()
        );
    }

    /// 会话拆除联动清除接入凭证（凭证随会话失效，防重放）。
    #[tokio::test]
    async fn teardown_clears_share_token() {
        let k = Kernel::new(Platform::Desktop);
        let s = k
            .create_session("a", &["b".into()], &SessionPrefs::default())
            .unwrap();
        let t = k
            .create_share_token(&s.id, vec![MediaKind::Mic], Duration::from_secs(60))
            .unwrap();
        assert!(k.verify_share_token(&t).is_ok());
        k.teardown(&s.id).unwrap();
        assert!(
            k.verify_share_token(&t).is_err(),
            "teardown 后凭证应失效（签发表移除）"
        );
    }

    /// 「可被发现」门控统一发现清单：`discoverable=false` 时 `/api/discovery`
    /// 不可见（子网单播扫描回退也探测不到），关闭 = 所有发现路径不可见。
    #[tokio::test]
    async fn discovery_manifest_gated_by_discoverable() {
        use crate::negotiator::DeviceIdentity;
        let k = Arc::new(Kernel::new(Platform::Desktop));
        k.set_identity(DeviceIdentity {
            device_id: "dev-gated".into(),
            device_name: "pico".into(),
        });
        let _ = k.start_relay_on(0, "pico").await.unwrap();
        // 默认 discoverable=false → 清单不可见
        assert!(
            k.discovery_manifest().is_none(),
            "可被发现默认关闭时不应对外提供发现清单"
        );
        // 开启 → 清单可见（mDNS + 子网扫描都据此找到本节点）
        k.set_discoverable(true);
        let m = k.discovery_manifest().expect("开启后可被发现应返回清单");
        assert_eq!(m.device_id, "dev-gated");
        assert!(m.relay_port > 0, "已锚定中继才有入口");
        // 再关闭 → 清单重新不可见
        k.set_discoverable(false);
        assert!(k.discovery_manifest().is_none());
    }

    // -----------------------------------------------------------------------
    // 多端点链接接收（通信模式 v2 Phase C「接收端多流化」）
    // -----------------------------------------------------------------------

    /// 推流辅助：WS 建流 + 关键帧（载荷带区分字节，供断言「哪条流」）。
    async fn push_keyframe_payload(
        base: &str,
        stream_id: &str,
        payload: Vec<u8>,
    ) -> Box<dyn crate::DataSession> {
        use crate::transport::{PeerAddr, SessionParams, Transport};
        use stross_proto::frame::{CODEC_H264, FLAG_KEYFRAME, TRACK_VIDEO};
        use stross_proto::message::ControlMessage;
        let transport = crate::transport::ws::WsTransport::new();
        let peer = PeerAddr {
            transport: stross_proto::message::TransportId::Ws,
            addr: format!("{base}/ws/push"),
        };
        let params = SessionParams {
            session_id: stream_id.into(),
            profile: ReliabilityProfile::Lossless,
        };
        let push = transport.connect(&peer, &params).await.unwrap();
        push.send(crate::SessionPacket::Control(ControlMessage::Hello {
            stream_id: stream_id.into(),
            title: "多链路测试流".into(),
            video: None,
            audio: None,
            share_token: None,
        }))
        .await
        .unwrap();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), push.recv()).await {
                Ok(Ok(Some(crate::SessionPacket::Control(ControlMessage::Welcome { .. })))) => {
                    break;
                }
                Ok(Ok(Some(_))) => continue,
                Ok(Ok(None)) => panic!("推流连接提前关闭"),
                Ok(Err(e)) => panic!("推流 recv 错误: {e}"),
                Err(_) => panic!("等 Welcome 超时"),
            }
        }
        push.send(crate::SessionPacket::Media(Frame::new(
            TRACK_VIDEO,
            CODEC_H264,
            FLAG_KEYFRAME,
            0,
            payload,
        )))
        .await
        .unwrap();
        push
    }

    /// 等编码帧通道出现载荷等于 `expect` 的关键帧（区分流归属）。
    async fn recv_raw_payload(rx: &mut mpsc::Receiver<Frame>, expect: &[u8], label: &str) {
        loop {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Some(f)) if f.payload.as_ref() == expect => break,
                Ok(Some(_)) => continue,
                Ok(None) => panic!("链路 {label} 通道提前关闭"),
                Err(_) => panic!("链路 {label} 收期望载荷超时"),
            }
        }
    }

    /// 多端点链接：同进程同时接收两条流（不同 link_id），每条链独立收帧 /
    /// 统计 / 停止——停一条不级联另一条；旧单流 API（main 槽）保持
    /// 「启新停旧」兼容语义。
    #[tokio::test]
    async fn multi_link_receive_independent_start_stop() {
        let handle = crate::relay::RelayServer::start(0).await.unwrap();
        let base = format!("ws://127.0.0.1:{}", handle.port);
        let kernel = Kernel::new(Platform::Desktop);

        let push_a = push_keyframe_payload(&base, "stream-a", vec![0xaa; 8]).await;
        let push_b = push_keyframe_payload(&base, "stream-b", vec![0xbb; 8]).await;

        // 两条链路并发接收（多端点链接：不再「第二路停第一路」）
        let ra = kernel
            .start_receive_raw_link("link-a".into(), base.clone(), "stream-a".into())
            .await
            .expect("链路 a 启动");
        let rb = kernel
            .start_receive_raw_link("link-b".into(), base.clone(), "stream-b".into())
            .await
            .expect("链路 b 启动");
        let mut fa = ra.take_raw_frames().expect("链路 a 帧通道");
        let mut fb = rb.take_raw_frames().expect("链路 b 帧通道");

        // 每条链收到各自流的关键帧（互不串流）
        recv_raw_payload(&mut fa, &[0xaa; 8], "a").await;
        recv_raw_payload(&mut fb, &[0xbb; 8], "b").await;

        // 链路注册表：两条都在
        let links = kernel.receive_links();
        assert_eq!(links.len(), 2, "两条链路并存");
        assert!(links.iter().all(|l| l.stats.running), "两条都在接收中");

        // 停链路 a：链路 b 不受影响（不级联）
        kernel.stop_receive_link("link-a");
        let links = kernel.receive_links();
        assert_eq!(links.len(), 1, "停一条后只剩链路 b");
        assert_eq!(links[0].link_id, "link-b");
        assert!(links[0].stats.running);
        // 链路 b 仍能继续收帧（再推一帧）
        push_b
            .send(crate::SessionPacket::Media(Frame::new(
                stross_proto::frame::TRACK_VIDEO,
                stross_proto::frame::CODEC_H264,
                stross_proto::frame::FLAG_KEYFRAME,
                1,
                vec![0xbb; 8],
            )))
            .await
            .unwrap();
        recv_raw_payload(&mut fb, &[0xbb; 8], "b").await;
        assert!(
            kernel.receive_links()[0].stats.received >= 2,
            "链路 b 持续收帧"
        );

        // 停链路 b：注册表清空
        kernel.stop_receive_link("link-b");
        assert!(kernel.receive_links().is_empty());

        drop(push_a);
        drop(push_b);
        handle.stop().await;
    }

    /// 旧单流 API 兼容：`start_receive_raw` 落 main 槽，启新停旧；`stop_receive`
    /// 只停 main，不影响多链路并存。
    #[tokio::test]
    async fn legacy_main_slot_keeps_stop_old_semantics() {
        let handle = crate::relay::RelayServer::start(0).await.unwrap();
        let base = format!("ws://127.0.0.1:{}", handle.port);
        let kernel = Kernel::new(Platform::Desktop);

        let _push1 = push_keyframe_payload(&base, "legacy-1", vec![0x11; 4]).await;
        let _push2 = push_keyframe_payload(&base, "legacy-2", vec![0x22; 4]).await;

        // 先启一条多链路（并存验证：旧 API 不清它）
        let r_extra = kernel
            .start_receive_raw_link("extra".into(), base.clone(), "legacy-1".into())
            .await
            .unwrap();
        let mut fx = r_extra.take_raw_frames().unwrap();
        recv_raw_payload(&mut fx, &[0x11; 4], "extra").await;

        // 旧 API：main 槽启新停旧
        kernel
            .start_receive_raw(base.clone(), "legacy-1".into())
            .await
            .unwrap();
        let r1 = kernel
            .start_receive_raw(base.clone(), "legacy-2".into())
            .await
            .unwrap();
        let _ = r1; // 第二次启动应停掉第一次（main 槽单链）
        let links = kernel.receive_links();
        assert_eq!(links.len(), 2, "main + extra 并存");
        let main_stats = links.iter().find(|l| l.link_id == "main").unwrap();
        assert_eq!(
            main_stats.stats.received, 0,
            "main 槽收到的是 legacy-2 流（新链）"
        );

        // stop_receive 只停 main，extra 不受影响
        kernel.stop_receive();
        let links = kernel.receive_links();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].link_id, "extra");
        kernel.stop_receive_link("extra");
        assert!(kernel.receive_links().is_empty());
        handle.stop().await;
    }
}
