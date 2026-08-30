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
//! * 端点（[`endpoint`]）：单层端点表 + load/share 行为契约（端点自维护
//!   可挂载性，内核不做类型分派）与订阅事件
//! * 数据面接线（[`data_plane`]）：受控中继作为数据面后端
//!
//! 所有变更通过 [`KernelEvent`] 广播给 UI（替代轮询）。
//! 内核**零路径 / 零 OS 调用 / 零平台分支**（仅有播放能力可用性的
//! `target_os` 分支，属能力交付而非逻辑分支）。

pub mod auth;
pub mod data_plane;
pub mod endpoint;
pub mod graph;
pub mod session;

pub use auth::{AuthError, AuthPolicy, PinAuthPolicy};
pub use data_plane::{DataPlaneBackend, RelayDataPlane};
// 端点契约与端点实现（插件区 stross-endpoint）：本模块只保留注册表
// （EndpointRegistry / EndpointEntry / FileSource），路径经 stross_kernel 根部重导出。
pub use endpoint::{
    EndpointEntry, EndpointRegistration, EndpointRegistry, FileSource, NodeRegistration,
    UnifiedRegistry,
};
pub use graph::{NodeInfo, NodeRole, TransportAddr};
pub use session::{Negotiated, Session, SessionPrefs};
pub use stross_endpoint::{
    Endpoint, EndpointBase, FileEndpoint, MicEndpoint, Probe, ScreenEndpoint, SubscribeCtx,
    SystemAudioEndpoint, TargetKind,
};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use stross_endpoint::capture::CaptureBackend;
use stross_endpoint::contract::EndpointApp;
use stross_endpoint::file::FilePushOptions;
use stross_endpoint::pipeline::StreamConfig;
#[cfg(not(target_os = "android"))]
use stross_endpoint::playback::AudioOut;
use stross_endpoint::playback::RenderedFrame;
use stross_proto::frame::Frame;
use stross_proto::message::{
    CapabilityDescriptor, CodecId, Delivery, DiscoveryInfo, EndpointManifest, EndpointState,
    EndpointStrategy, MediaKind, RoutePath, ShareToken, StreamInfo, SubscribeSpec, TransportId,
    TransportPreference, Visibility,
};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::engine::SenderEngine;
use crate::error::{Error, Result};
use crate::lock::MutexExt;
use crate::negotiator::DeviceIdentity;
use crate::receiver::{LocalProxy, Receiver};
use crate::relay::{DEFAULT_PORT, RelayEvent, RelayHandle, RelayServer};
use crate::view;
use stross_proto::message::Platform;
use stross_types::{AppInfo, CaptureStatusView, DeviceList, RelayInfo, StartResult, StreamStatus};

use self::graph::DeviceGraph;
use self::session::{Router, SessionManager};

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
pub struct Kernel {
    // -- 控制面：设备图 / 会话 / 路由 / 鉴权 / 凭证 --
    graph: DeviceGraph,
    sessions: SessionManager,
    auth: Arc<dyn AuthPolicy>,
    next_id: AtomicU64,
    events: broadcast::Sender<KernelEvent>,
    /// 数据面后端（内嵌受控中继等；`None` = 未接线，会话不驱动数据面）。
    data_plane: Mutex<Option<Arc<dyn DataPlaneBackend>>>,
    /// 数据面事件转发任务（[`RelayEvent`] → [`KernelEvent`]）。
    data_plane_task: Mutex<Option<JoinHandle<()>>>,
    /// 接入凭证签发表（stream_id → 签发时的完整凭证；`Arc` 供数据面校验器共享，
    /// 校验器只持本表引用，不形成循环引用）。
    share_tokens: Arc<Mutex<HashMap<String, ShareToken>>>,
    // -- 运行态：平台标签 / 推流 / 接收 / 端点 / 身份 --
    /// 平台标签（纯值；设备能力枚举等平台知识在 stross-bridge）。
    platform: Platform,
    /// 运行中推流（`Arc` 供数据面事件转发任务共享：流结束时清理引擎状态）。
    /// **并发流**：端点模型的「任意端点可推送/订阅」目标要求一个节点能同时推多路
    /// 流（如屏幕 + 系统声音），故按 `stream_id` 管理多台推流引擎（原来是单引擎）。
    engines: Arc<Mutex<HashMap<String, RunningStream>>>,
    /// 本机锚点：常驻受控中继 + mDNS 广播（免先连：一起启动、生命周期一致）。
    anchor: Mutex<Option<LocalAnchor>>,
    /// 「可被发现」（mDNS 广播本机）：显式用户开关，默认**关**。
    /// 开启时锚定中继才广播 mDNS、通告端点会即时刷新 TXT 摘要；
    /// 关闭时本机不被局域网 mDNS 扫描发现（设备仍可直连协商）。
    discoverable: AtomicBool,
    /// 采集后端（平台相关，UI 层注入；`Arc` 使其可被引擎复用）。
    backend: Mutex<Option<Arc<dyn CaptureBackend>>>,
    /// 端点框架：**三层统一注册表**（节点 → 端点 → 策略；本机 + 互联节点
    /// 同一张表，docs/endpoint-model-v2.md §2）。
    registry: Mutex<UnifiedRegistry>,
    /// 端点共享登记（stream_id → 端点）：实时目标生命周期治理——
    /// watchers=0 自动收尾 / 取消通告联动停止 / 同端点订阅收敛（iteration-plan.md 第十二轮）。
    active_shares: Mutex<HashMap<String, ActiveShare>>,
    /// watchers 归零后停止端点共享的延迟（给订阅者重连 / 新订阅者接入窗口；测试注入）。
    share_stop_delay: Duration,
    /// 端点共享启动后无任何观看者的接入窗口（订阅者从未接入时兜底停止）。
    share_idle_delay: Duration,
    /// 接收播放：WS 收流 → 抖动缓冲 → 播放/解码（Android 走 raw 帧路径）。
    receiver: Mutex<Option<Arc<Receiver>>>,
    /// 本机持久化身份（`load_or_create_identity` 注入；用于 mDNS 实例名
    /// 唯一化——多设备同端口广播不再同名串扰）。
    identity: Mutex<Option<DeviceIdentity>>,
    /// 实例启动时刻（控制面 Status 的 uptime 统计源）。
    started: std::time::Instant,
}

/// 本机锚点（免先连：应用打开即自动建立；推流 / 观看 / 局域网发现共用）。
struct LocalAnchor {
    handle: RelayHandle,
    /// mDNS 广播句柄（`None` = 未广播：启动时未开可被发现，或广播失败）。
    /// [`apply_discoverable`] 按开关收敛其生命周期（开启建 / 关闭停），
    /// 不再当作常驻随手持有。
    discovery: Option<crate::discovery::Discovery>,
    /// 中继实际监听端口（绑定 0 自动分配时取实际值）。
    port: u16,
    /// 广播主机名（重注册 / 刷新摘要需要；由锚定流程注入）。
    hostname: String,
}

/// 运行中的推流。
struct RunningStream {
    engine: SenderEngine,
    relay_port: u16,
    title: String,
    stream_id: String,
    started_at: u64,
}

/// 端点共享登记条目（实时目标；文件端点不登记——有完成态，StreamEnded 统一清理）。
#[derive(Debug, Clone)]
struct ActiveShare {
    endpoint_id: String,
    delivery: Delivery,
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
            receiver: Mutex::new(None),
            identity: Mutex::new(None),
            started: std::time::Instant::now(),
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
    pub fn seed_endpoint(&self, ep: Box<dyn Endpoint>) {
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
    // 局域网可发现（mDNS 广播本机：显式用户开关）
    // -----------------------------------------------------------------------

    /// 当前是否可被发现（mDNS 广播本机）。默认关。
    pub fn discoverable(&self) -> bool {
        self.discoverable.load(Ordering::Relaxed)
    }

    /// 启用/关闭可被发现（mDNS 广播本机）。
    ///
    /// 开启时：若已锚定中继，立即广播本机（首次则新建句柄，已广播仅刷新
    /// TXT 摘要——通告状态可能已变）；关闭时：停止本机广播。未锚定仅记状态，
    /// 锚定流程按此状态生效。
    pub fn set_discoverable(&self, on: bool) {
        self.discoverable.store(on, Ordering::Relaxed);
        self.apply_discoverable();
    }

    /// 按当前 `discoverable` 状态收敛 mDNS 广播（锚定 / 端点通告后调用）。
    ///
    /// **锁序**：先取 anchor 锁，再在 [`Self::mdns_info`] 里取 registry 锁。
    /// 反向序（registry → anchor）不存在；锚定流程锚定时不持 registry 锁、
    /// 通告流程只持 registry 锁并在锁外调本方法，故无死锁。
    fn apply_discoverable(&self) {
        let on = self.discoverable.load(Ordering::Relaxed);
        let mut anchor = self.anchor.lock_poisoned();
        let Some(a) = anchor.as_mut() else {
            return; // 未锚定：仅记状态
        };
        if on {
            // 开启：未广播则新建句柄（try_register_mdns 内部构建摘要）；
            // 已广播则重注册刷新 TXT（端点摘要可能已变）。
            if a.discovery.is_none() {
                a.discovery = self.try_register_mdns(&a.hostname, a.port);
            } else if let Some(d) = a.discovery.as_mut()
                && let Err(e) = d.redefine(&self.mdns_info(&a.hostname))
            {
                tracing::warn!("mDNS 刷新失败: {e}");
            }
        } else {
            // 关闭：停止本机广播（句柄 Drop 即反注册）
            if let Some(mut d) = a.discovery.take() {
                d.stop();
            }
        }
    }

    /// 构造本机 mDNS 能力描述（`DiscoveryInfo`；端点摘要取当前注册表快照）。
    fn mdns_info(&self, hostname: &str) -> DiscoveryInfo {
        DiscoveryInfo::relay_default(
            hostname.to_string(),
            vec![
                MediaKind::Screen,
                MediaKind::Camera,
                MediaKind::Mic,
                MediaKind::SystemAudio,
            ],
        )
        .with_endpoints(self.registry.lock_poisoned().summaries())
    }

    /// 注册 mDNS 广播本机中继；失败告警并返回 `None`（中继仍可用）。
    fn try_register_mdns(&self, hostname: &str, port: u16) -> Option<crate::discovery::Discovery> {
        let instance = relay_mdns_instance(
            self.identity
                .lock_poisoned()
                .as_ref()
                .map(|id| id.device_id.as_str()),
            port,
        );
        match crate::discovery::Discovery::start(
            &instance,
            &crate::net::local_ips(),
            port,
            &self.mdns_info(hostname),
            hostname,
        ) {
            Ok(d) => {
                tracing::info!("mDNS 广播已开启（可被发现）: {instance}");
                Some(d)
            }
            Err(e) => {
                tracing::warn!("mDNS 广播失败: {e}");
                None
            }
        }
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
    // 端点框架（三层端点模型：节点 → 端点 → 策略，见 docs/endpoint-model-v2.md）
    // -----------------------------------------------------------------------

    /// 通告端点为可订阅（可见性 / delivery / 传输由公开者声明）。
    ///
    /// 不可挂载端点（`available=false`）拒绝通告，错误携带 load 探测原因
    /// （如「无图形会话」——屏幕获取失败前置化）。`transports` / `codecs`
    /// 缺省时按端点**目标类型**给默认（实时目标 Lossy → QUIC>SRT>WS，
    /// 确定目标 Lossless → QUIC>WS）。
    pub fn publish_endpoint(
        &self,
        endpoint_id: &str,
        visibility: Visibility,
        delivery: Delivery,
        transports: Option<Vec<TransportPreference>>,
        codecs: Option<Vec<CodecId>>,
    ) -> Result<EndpointManifest> {
        let manifest = {
            let mut reg = self.registry.lock_poisoned();
            let target = reg.target(endpoint_id).unwrap_or(TargetKind::Live);
            let transports =
                transports.unwrap_or_else(|| EndpointRegistry::default_transports(target));
            let codecs = codecs.unwrap_or_else(|| vec![CodecId::H264, CodecId::Aac]);
            reg.publish(endpoint_id, visibility, delivery, transports, codecs)?
        };
        // 通告 → 立即刷新 mDNS 端点摘要（可被发现时；锁外，避免 registry→anchor 反序）
        self.apply_discoverable();
        Ok(manifest)
    }

    /// 取消通告端点（端点保留在表里可再次通告；已订阅会话由上层决定宽限期）。
    ///
    /// **活动共享联动**：该端点若正在被订阅观看，先停止共享并拆除会话
    /// （取消通告 = 不再共享，踢出当前订阅者）。
    pub async fn unpublish_endpoint(&self, endpoint_id: &str) -> Result<()> {
        self.stop_endpoint_share(endpoint_id)?;
        self.registry.lock_poisoned().unpublish(endpoint_id)?;
        // 取消通告 → 立即刷新 mDNS 端点摘要（锁外）
        self.apply_discoverable();
        Ok(())
    }

    /// 公开本地文件为文件端点（动态端点 `file:<名>`；本地路径登记但不出现在
    /// 目录 / 摘要 / wire，见 docs/endpoint-model-v2.md §3）。
    pub fn publish_file_endpoint(
        &self,
        path: &Path,
        visibility: Visibility,
        delivery: Delivery,
    ) -> Result<EndpointManifest> {
        let manifest = self
            .registry
            .lock_poisoned()
            .publish_file(path, visibility, delivery)?;
        // 通告 → 立即刷新 mDNS 端点摘要（锁外）
        self.apply_discoverable();
        Ok(manifest)
    }

    /// 文件端点的本地文件源（control.rs 状态展示）。
    pub fn file_source(&self, endpoint_id: &str) -> Option<FileSource> {
        self.registry
            .lock_poisoned()
            .file_source(endpoint_id)
            .cloned()
    }

    /// 订阅达成事件（协商层授予成功后调用）：触发端点 `share`（端点自驱动，
    /// 内核不做类型分派）。
    ///
    /// share 在注册表锁**外**调用（端点实现会再次访问内核），持锁回调会死锁。
    pub fn on_endpoint_subscribed(&self, app: Arc<Self>, endpoint_id: &str, ctx: &SubscribeCtx) {
        self.registry
            .lock_poisoned()
            .on_subscribed(&app, endpoint_id, ctx);
    }

    /// 端点清单查询（订阅握手 / 目录 API 用）。
    pub fn endpoint_manifest(&self, endpoint_id: &str) -> Option<EndpointManifest> {
        self.registry.lock_poisoned().manifest(endpoint_id)
    }

    /// 目录快照：全部端点清单（Private / 未通告可见性过滤由调用方做）。
    pub fn endpoint_catalog(&self) -> Vec<EndpointManifest> {
        self.registry.lock_poisoned().manifests()
    }

    /// 已通告端点清单（对端目录用；Private 过滤由协商层做）。
    pub fn published_endpoints(&self) -> Vec<EndpointManifest> {
        self.registry.lock_poisoned().published_manifests()
    }

    /// 本机目录视图（全部端点；节点卡片端点树渲染用）。
    pub fn local_catalog(&self) -> stross_types::LocalCatalog {
        let endpoints = self.endpoint_catalog();
        stross_types::LocalCatalog { endpoints }
    }

    // -----------------------------------------------------------------------
    // 统一注册表（v2 三层：节点 → 端点 → 策略；docs/endpoint-model-v2.md §2）
    // -----------------------------------------------------------------------

    /// 把目录响应（`GET /api/endpoints`）的互联节点映射进统一注册表
    /// （节点 → 端点 → 策略）。订阅方拉取目录后调用——与 mDNS 摘要不同，
    /// 目录携带完整策略组合（序列化 + pick）。
    pub fn register_remote_directory(&self, dir: &stross_proto::message::EndpointDir, addr: &str) {
        self.registry
            .lock_poisoned()
            .register_remote_directory(dir, addr);
    }

    /// 统一查表：`registry[节点][端点][策略]` → 策略组合。
    /// 自订（本机节点）与订其它互联节点走同一套逻辑；`strategy_id` 缺省 =
    /// 端点默认策略。
    pub fn resolve_strategy(
        &self,
        node_id: &str,
        endpoint_id: &str,
        strategy_id: Option<&str>,
    ) -> Option<EndpointStrategy> {
        self.registry
            .lock_poisoned()
            .resolve_strategy(node_id, endpoint_id, strategy_id)
    }

    /// 三层注册表快照（节点 → 端点 → 策略；含本机镜像；UI / 调试用）。
    pub fn registry_nodes(&self) -> Vec<NodeRegistration> {
        self.registry.lock_poisoned().node_registrations()
    }

    /// 订阅端点生成 + 委托（v2 订阅端，docs/endpoint-model-v2.md §3）：
    /// 从注册表 `(节点, 端点, 策略)` 生成订阅端点并调其 `subscribe`——
    /// 与分享端 `share` 同构（端点自驱动，内核不分派）。订阅目标类型暂无
    /// 订阅端点宿主时返回错误（媒体播放由接收链路承担）。
    pub fn subscribe_via_endpoint(
        &self,
        app: Arc<Self>,
        spec: &SubscribeSpec,
        out_dir: Option<&Path>,
    ) -> Result<()> {
        let ep = self
            .registry
            .lock_poisoned()
            .generate_subscribe_endpoint(spec, out_dir)
            .ok_or_else(|| {
                Error::Message(format!(
                    "端点「{}」的订阅目标类型暂无订阅端点宿主（生成订阅端点失败）",
                    spec.endpoint_id
                ))
            })?;
        ep.subscribe(app, spec.clone());
        Ok(())
    }

    // -----------------------------------------------------------------------
    // 端点共享生命周期（iteration-plan.md 第十二轮）
    // -----------------------------------------------------------------------

    /// 端点共享登记（媒体端点 `start_stream` 成功后由端点层回调，
    /// 见 [`EndpointApp::note_share_active`]）：登记 + 状态置 Active +
    /// 启动"无观看者接入窗口"兜底检查（订阅者从未接入时停止）。
    pub fn note_share_active(
        &self,
        self_weak: std::sync::Weak<dyn EndpointApp>,
        endpoint_id: &str,
        stream_id: &str,
        delivery: Delivery,
    ) {
        {
            let mut shares = self.active_shares.lock_poisoned();
            shares.insert(
                stream_id.to_string(),
                ActiveShare {
                    endpoint_id: endpoint_id.to_string(),
                    delivery,
                },
            );
        }
        let _ = self
            .registry
            .lock_poisoned()
            .set_state(endpoint_id, EndpointState::Active, 0);
        tracing::info!("端点共享已登记: {endpoint_id} → {stream_id} ({delivery:?})");
        // 接入窗口兜底：与事件顺序无关（StreamStarted 可能先于登记到达转发任务），
        // 因此在登记处统一启动检查（经弱引用回调，不拖住内核）。
        let stream_id = stream_id.to_string();
        let idle = self.share_idle_delay;
        tokio::spawn(async move {
            tokio::time::sleep(idle).await;
            if let Some(app) = self_weak.upgrade() {
                app.stop_share_if_unwatched(&stream_id);
            }
        });
    }

    /// 停止指定端点的活动共享（幂等：无活动共享时直接成功）。
    pub fn stop_endpoint_share(&self, endpoint_id: &str) -> Result<()> {
        let sid = self
            .active_shares
            .lock_poisoned()
            .iter()
            .find_map(|(sid, s)| (s.endpoint_id == endpoint_id).then(|| sid.clone()));
        let Some(sid) = sid else {
            return Ok(()); // 无活动共享
        };
        self.stop_share_by_stream(&sid);
        Ok(())
    }

    /// 按流停止端点共享：清登记 + 复位状态 + 优雅停流 + 拆除本机会话。
    /// （同步：停流仅取出引擎并 spawn 收尾，不在本路径 await。）
    fn stop_share_by_stream(&self, stream_id: &str) {
        // 先取走登记（并发到达的停止请求只执行一次）
        let Some(share) = self.clear_active_share(stream_id) else {
            return;
        };
        tracing::info!("端点共享停止: {} (stream={stream_id})", share.endpoint_id);
        let _ = self
            .registry
            .lock_poisoned()
            .set_state(&share.endpoint_id, EndpointState::Idle, 0);
        // 优雅停流：按 stream_id 从并发流表取出对应引擎，仅在存在时动作
        if let Some(stream) = self.engines.lock_poisoned().remove(stream_id) {
            tokio::spawn(async move {
                stream.engine.stop().await;
            });
        }
        // 拆除本机会话（会话生命周期 = 流生命周期；远程 push 会话不在本机，
        // SessionNotFound 忽略）
        if self.has_session(stream_id) {
            let _ = self.teardown(stream_id);
        }
    }

    /// watchers 归零复查：仍无人观看才停（期间有新观众接入则放弃）。
    fn stop_share_if_unwatched(&self, stream_id: &str) {
        let Some(dp) = self.data_plane.lock_poisoned().clone() else {
            return;
        };
        // 流已消失（StreamEnded 路径清理）或有观众接入时不动
        if let Some(0) = dp.stream_watchers(stream_id) {
            self.stop_share_by_stream(stream_id);
        }
    }

    /// 生命周期治理延迟（默认 stop 4s / idle 10s；测试与嵌入式调用方可按需收紧）。
    pub fn set_share_lifecycle_delays(&mut self, stop_delay: Duration, idle_delay: Duration) {
        self.share_stop_delay = stop_delay;
        self.share_idle_delay = idle_delay;
    }

    /// 查询端点当前活动共享（`(stream_id, delivery)`；订阅收敛用）。
    pub fn active_share_by_endpoint(&self, endpoint_id: &str) -> Option<(String, Delivery)> {
        self.active_shares
            .lock_poisoned()
            .iter()
            .find_map(|(sid, s)| (s.endpoint_id == endpoint_id).then(|| (sid.clone(), s.delivery)))
    }

    /// 查询流的活动共享登记（watchers 事件反查端点用）。
    fn active_share_by_stream(&self, stream_id: &str) -> Option<ActiveShare> {
        self.active_shares.lock_poisoned().get(stream_id).cloned()
    }

    /// 取走流的活动共享登记（停止 / 流结束时调用）。
    fn clear_active_share(&self, stream_id: &str) -> Option<ActiveShare> {
        self.active_shares.lock_poisoned().remove(stream_id)
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
        *self.data_plane.lock_poisoned() = Some(backend.clone());
        backend.set_share_token_validator(self.token_validator());
        let mut rx = backend.events();
        let events = self.events.clone();
        let engines = Arc::clone(&self.engines);
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
                        // 1) 端点共享登记清理 + 状态复位（推流端断开 / 静默超时 /
                        //    显式停止 / revoke 都可能触发）
                        if let Some(share) = me.clear_active_share(&stream_id) {
                            me.registry.lock_poisoned().set_state(
                                &share.endpoint_id,
                                EndpointState::Idle,
                                0,
                            );
                        }
                        // 2) 并发推流引擎状态清理：按 stream_id 移除对应引擎
                        //    （防采集进程中途退出后该流残留、卡住同 id 重推）
                        if let Some(dead) = engines.lock_poisoned().remove(&stream_id) {
                            tokio::spawn(async move {
                                dead.engine.stop().await;
                            });
                        }
                        // 3) 本机会话随流结束拆除（无 PIN 会话直接放行；
                        //    远程 push 会话不在本机 → SessionNotFound 忽略）
                        if me.has_session(&stream_id) {
                            let _ = me.teardown(&stream_id);
                        }
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
                        if watchers == 0 && me.active_share_by_stream(&stream_id).is_some() {
                            let me2 = me.clone();
                            let sid = stream_id.clone();
                            let delay = stop_delay;
                            tokio::spawn(async move {
                                tokio::time::sleep(delay).await;
                                me2.stop_share_if_unwatched(&sid);
                            });
                        }
                        // 订阅数同步：端点共享的流 subscribers = watchers
                        if let Some(share) = me.active_share_by_stream(&stream_id) {
                            me.registry.lock_poisoned().set_state(
                                &share.endpoint_id,
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
        *self.data_plane_task.lock_poisoned() = Some(task);
    }

    /// 是否已接入数据面。
    pub fn has_data_plane(&self) -> bool {
        self.data_plane.lock_poisoned().is_some()
    }

    // -----------------------------------------------------------------------
    // 接入凭证（B 阶段：凭证式跨设备推流，见 docs/iteration-plan.md B0/B1）
    // -----------------------------------------------------------------------

    /// 为已建会话签发一次性接入凭证（`ttl` 为有效期；`media` 为本次共享类型）。
    ///
    /// 调用方（控制面 / GUI）把凭证编码为二维码 / 短码交给推流端（如手机）；
    /// 推流端在 Hello 中出示凭证即可接入本机受控中继，**无需任何远程控制面**。
    pub fn create_share_token(
        &self,
        session_id: &str,
        media: Vec<MediaKind>,
        ttl: Duration,
    ) -> Result<ShareToken> {
        if !self.has_session(session_id) {
            return Err(Error::SessionNotFound(session_id.to_string()));
        }
        let now = now_secs();
        // 惰性清理过期凭证，保持签发表有界
        let mut tokens = self.share_tokens.lock_poisoned();
        tokens.retain(|_, t| !t.is_expired(now));
        let token = ShareToken {
            v: ShareToken::VERSION,
            stream_id: session_id.to_string(),
            pin: random_pin(session_id),
            expires_at: now.saturating_add(ttl.as_secs()),
            media,
        };
        tokens.insert(session_id.to_string(), token.clone());
        Ok(token)
    }

    /// 校验凭证：已签发 + 未过期 + 与签发时逐字一致（防篡改 / 重放）。
    pub fn verify_share_token(&self, token: &ShareToken) -> Result<()> {
        let tokens = self.share_tokens.lock_poisoned();
        let stored = tokens.get(&token.stream_id).ok_or_else(|| {
            Error::Token(format!("凭证无效：会话 {} 未签发凭证", token.stream_id))
        })?;
        if stored != token {
            return Err(Error::Token(
                "凭证无效：与签发时不符（可能被篡改或重放）".into(),
            ));
        }
        if stored.is_expired(now_secs()) {
            return Err(Error::Token("凭证已过期".into()));
        }
        Ok(())
    }

    /// 数据面凭证校验器（读本内核签发表；注入受控中继用）。
    pub fn token_validator(&self) -> Arc<dyn crate::relay::ShareTokenValidator> {
        Arc::new(KernelTokenValidator {
            tokens: self.share_tokens.clone(),
        })
    }

    // -----------------------------------------------------------------------
    // 设备图
    // -----------------------------------------------------------------------

    /// 注册/更新一个节点（发现结果、本机能力都走这里）。
    pub fn upsert_node(&self, node: NodeInfo) {
        self.graph
            .nodes
            .lock_poisoned()
            .insert(node.node_id.clone(), node);
    }

    /// 给已有节点追加一条能力（重复条目按 `media` 去重）。
    pub fn register_capability(&self, node_id: &str, desc: CapabilityDescriptor) {
        let mut guard = self.graph.nodes.lock_poisoned();
        if let Some(node) = guard.get_mut(node_id)
            && !node.caps.contains(&desc)
        {
            node.caps.push(desc);
        }
    }

    /// 当前设备图快照（按节点 id 排序）。
    pub fn nodes(&self) -> Vec<NodeInfo> {
        let guard = self.graph.nodes.lock_poisoned();
        let mut v: Vec<_> = guard.values().cloned().collect();
        v.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        v
    }

    // -----------------------------------------------------------------------
    // 会话
    // -----------------------------------------------------------------------

    /// 创建会话（「从 `src` 推送到 `sinks`」）。
    ///
    /// 根据源节点能力做**最简协商**（传输偏好 ∩ 源能力、编解码取源能力
    /// 第一项），填充 [`Session::negotiated`]；完整的线上 Offer/Answer 在
    /// 传输信令层完成（如 WebRTC 的 `/api/webrtc/*`）。
    ///
    /// 已接入数据面（[`Kernel::attach_data_plane`]）时，会话 id 由内核签发并
    /// **预授权**给受控中继（需求 F2.2「先会话后传输」/ D4：id 与 stream_id 合一）。
    pub fn create_session(
        &self,
        src: &str,
        sinks: &[String],
        prefs: &SessionPrefs,
    ) -> Result<Session> {
        if sinks.is_empty() {
            return Err(Error::Message("会话至少需要一个接收端（sinks）".into()));
        }
        let id = format!("sess-{:x}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let requires_pin = prefs.access_code.is_some();
        if requires_pin {
            self.auth.set_code(&id, prefs.access_code.as_deref());
        }
        // 数据面预授权：先授权成功再登记会话，避免"会话已建但无法推流"的中间态
        // （先 clone 出 Arc 再调用外部后端，不在持内核锁期间执行后端调用）
        let dp = self.data_plane.lock_poisoned().clone();
        if let Some(dp) = dp {
            dp.authorize_stream(&id)
                .map_err(|e| Error::DataPlane(format!("预授权失败: {e}")))?;
        }
        let session = Session {
            id,
            title: prefs.title.clone(),
            source: src.to_string(),
            sinks: sinks.to_vec(),
            path: Router::default_path(sinks),
            negotiated: self.negotiate(src, prefs),
            requires_pin,
            authorized: !requires_pin, // 无访问码的会话控制面直接放行（现状行为）
        };
        self.sessions.insert(session.clone());
        let _ = self.events.send(KernelEvent::SessionStarted {
            session: session.clone(),
        });
        Ok(session)
    }

    /// 控制传输方向：会话存续期间动态改道。
    ///
    /// 会话启用访问码（PIN）且未通过 [`Kernel::authorize`] 时拒绝（设计文档 §7）。
    pub fn route(&self, id: &str, path: RoutePath) -> Result<()> {
        self.sessions.route(id, path.clone())?;
        let _ = self.events.send(KernelEvent::SessionRouted {
            session_id: id.to_string(),
            path,
        });
        Ok(())
    }

    /// 会话鉴权：校验访问码；成功后该会话的控制操作放行。
    ///
    /// 未设置访问码的会话直接成功（无操作）。
    pub fn authorize(&self, id: &str, access_code: Option<&str>) -> Result<()> {
        self.auth.authorize(id, access_code)?;
        self.sessions.mark_authorized(id)
    }

    /// 查询单个会话。
    pub fn session(&self, id: &str) -> Option<Session> {
        self.sessions.get(id)
    }

    /// 会话列表快照（按 id 排序）。
    pub fn sessions(&self) -> Vec<Session> {
        self.sessions.snapshot()
    }

    /// 会话是否存在（id 已由内核签发且未拆除）。
    pub fn has_session(&self, id: &str) -> bool {
        self.sessions.contains(id)
    }

    /// 最简能力协商：
    /// * 传输：`prefs.preferred_transport` ∩ 源能力；未指定时源支持 webrtc 则用
    ///   webrtc，否则 ws（推流现状）
    /// * 编解码：源能力第一项（默认 h264）
    fn negotiate(&self, src: &str, prefs: &SessionPrefs) -> Negotiated {
        let caps: Vec<CapabilityDescriptor> = self
            .graph
            .nodes
            .lock_poisoned()
            .get(src)
            .map(|n| n.caps.clone())
            .unwrap_or_default();
        let mut transports: Vec<TransportId> = caps
            .iter()
            .flat_map(|c| c.transports.iter().copied())
            .collect();
        transports.sort();
        transports.dedup();
        let transport = match &prefs.preferred_transport {
            Some(t) if transports.is_empty() || transports.contains(t) => *t,
            _ => {
                if transports.contains(&TransportId::WebRtc) {
                    TransportId::WebRtc
                } else {
                    TransportId::Ws
                }
            }
        };
        let codec = caps
            .iter()
            .flat_map(|c| c.codecs.iter().copied())
            .next()
            .unwrap_or(CodecId::H264);
        Negotiated {
            transport,
            codec,
            profile: prefs.profile,
        }
    }

    /// 拆除会话（同样受访问码鉴权约束）；已接入数据面时撤销流预授权。
    pub fn teardown(&self, id: &str) -> Result<()> {
        self.sessions.require_authorized(id)?;
        self.sessions.remove(id);
        self.auth.set_code(id, None); // 清理访问码
        self.share_tokens.lock_poisoned().remove(id); // 凭证随会话失效（防重放）
        let dp = self.data_plane.lock_poisoned().clone();
        if let Some(dp) = dp {
            dp.revoke_stream(id)
                .map_err(|e| Error::DataPlane(format!("撤销失败: {e}")))?;
        }
        let _ = self.events.send(KernelEvent::SessionEnded {
            session_id: id.to_string(),
        });
        Ok(())
    }

    /// 订阅内核事件。
    pub fn subscribe(&self) -> broadcast::Receiver<KernelEvent> {
        self.events.subscribe()
    }

    // -----------------------------------------------------------------------
    // 连接阶段（锚点：先连接，再推流/观看）
    // -----------------------------------------------------------------------

    /// 启动/复用本机中继（"先连接"步骤的本机选项）。
    ///
    /// 本机中继以**受控模式**启动（需求 F2.2「先会话后传输」）：只有内核
    /// 创建的会话 id 才能推流；中继作为数据面后端接入内核，
    /// 流生命周期事件转发为 [`KernelEvent`]。
    pub async fn start_relay(self: &Arc<Self>, hostname: &str) -> Result<RelayInfo> {
        self.start_relay_on(DEFAULT_PORT, hostname).await
    }

    /// 在指定端口启动中继（受内核控制的数据面）；被占用时回退随机端口。
    pub async fn start_relay_on(self: &Arc<Self>, port: u16, hostname: &str) -> Result<RelayInfo> {
        self.start_relay_fixed(port, 0, 0, hostname).await
    }

    /// 在指定端口启动中继，并固定 SRT/QUIC 传输端口（`0` = 随机）。
    ///
    /// 固定端口便于防火墙只放行已知端口（权限自动化）；SRT/QUIC 被占用时
    /// 各自回退随机端口（实际端口经 `/api/info` 可见）。
    ///
    /// `hostname`：mDNS 广播主机名（**调用方注入**——内核零 OS 调用；
    /// 壳层经 [`stross_bridge::hostname`] 取值）。
    pub async fn start_relay_fixed(
        self: &Arc<Self>,
        port: u16,
        srt_port: u16,
        quic_port: u16,
        hostname: &str,
    ) -> Result<RelayInfo> {
        {
            let guard = self.anchor.lock_poisoned();
            if let Some(a) = guard.as_ref() {
                return Ok(view::relay_info(
                    a.port,
                    hostname,
                    self.registry.lock_poisoned().summaries(),
                ));
            }
        } // 优先指定端口；被占用时回退随机端口（本机中继"能用就行"，不因端口冲突失败）
        let handle =
            if let Ok(h) = RelayServer::start_controlled_with(port, srt_port, quic_port).await {
                h
            } else {
                tracing::warn!("端口 {port} 被占用，本机中继回退到随机端口");
                RelayServer::start_controlled(0).await?
            };
        let port = handle.port;
        // 中继接入内核（数据面后端）：订阅流事件、会话预授权
        self.attach_data_plane(Arc::new(RelayDataPlane::new(&handle)));
        // 把本机注册进内核设备图（含采集能力，供会话协商）
        self.register_local_node(hostname);
        // mDNS 广播本机中继：**仅当「可被发现」开启时**才广播（显式用户开关，
        // 默认关）。开启由 `set_discoverable(true)` 触发（或锚定前已开）。
        // 能力描述统一走 DiscoveryInfo 单 key JSON（F1.2 / 1d）；多网卡广播
        // 全部局域网 IP（Discovery::start 内部处理空列表回退回环），避免只广播
        // 第一个 IP 导致其它网卡网段扫描不到本机。
        let hostname = hostname.to_string();
        let discovery = if self.discoverable.load(Ordering::Relaxed) {
            self.try_register_mdns(&hostname, port)
        } else {
            None
        };
        *self.anchor.lock_poisoned() = Some(LocalAnchor {
            handle,
            discovery,
            port,
            hostname: hostname.clone(),
        });
        Ok(view::relay_info(
            port,
            &hostname,
            self.registry.lock_poisoned().summaries(),
        ))
    }

    /// 把本机节点（含采集能力）注册进内核设备图。
    pub fn register_local_node(&self, hostname: &str) {
        self.upsert_node(NodeInfo {
            node_id: "local".into(),
            name: hostname.into(),
            roles: vec![NodeRole::Sender, NodeRole::Viewer, NodeRole::Relay],
            caps: vec![],
            addrs: vec![],
        });
        if let Some(backend) = self.backend.lock_poisoned().as_ref() {
            self.register_capability("local", backend.descriptor());
        }
    }

    // -----------------------------------------------------------------------
    // 推流
    // -----------------------------------------------------------------------

    /// 开始推流。
    ///
    /// * `cfg`：采集配置（视频源 / 画质 / 音频）
    /// * `relay_url`：`Some` 推到指定中继（ws:///srt:///quic://，按 scheme 选传输）；
    ///   `None` 推到常驻本机中继，地址按流媒体类型自动选传输
    ///
    /// 已接入数据面（本机受控中继）时，若 `cfg.stream_id` 还不是内核会话
    /// （旧 UI 直接推流的兜底），自动创建本机会话并由内核签发 id（D4）；
    /// 新 UI 应先 `create_session` 再传对应 id。
    pub async fn start_stream(
        &self,
        mut cfg: StreamConfig,
        relay_url: Option<String>,
    ) -> Result<StartResult> {
        // 并发推流：端点模型允许同一节点同时推多路流（屏幕 + 系统声音等），
        // 不再有「已经在推流中」的单流限制；仅同一 stream_id 重复启动则拒绝。
        let backend = self
            .backend
            .lock_poisoned()
            .clone()
            .ok_or_else(|| Error::Message("采集后端未初始化".into()))?;
        // 会话兜底：受控中继只接受内核会话 id；未建会话时自动创建
        self.ensure_session(&mut cfg)?;
        // 未指定中继时，推到已连接（常驻）的本机中继；
        // 推流地址按媒体类型自动选传输（视频→SRT>QUIC>WS，纯音频→QUIC>WS）
        let relay_url = if let Some(u) = relay_url {
            Some(u)
        } else {
            let guard = self.anchor.lock_poisoned();
            guard
                .as_ref()
                .map(|a| a.handle.auto_push_url(cfg.video.is_some()))
        };
        let engine =
            SenderEngine::start(cfg.clone(), backend, relay_url.clone(), DEFAULT_PORT).await?;
        // 有效中继端口：内嵌中继 > 常驻中继 > 默认端口
        let relay_port = engine
            .relay_port()
            .or_else(|| self.anchor.lock_poisoned().as_ref().map(|a| a.port))
            .unwrap_or(DEFAULT_PORT);
        let started_at = stross_proto::time::unix_secs();
        {
            let mut g = self.engines.lock_poisoned();
            if g.contains_key(&cfg.stream_id) {
                return Err(Error::Message("该流已在推流中".into()));
            }
            g.insert(
                cfg.stream_id.clone(),
                RunningStream {
                    engine,
                    relay_port,
                    title: cfg.title.clone(),
                    stream_id: cfg.stream_id.clone(),
                    started_at,
                },
            );
            tracing::info!("推流开始: {} (并发推流数={})", cfg.stream_id, g.len());
        }
        Ok(StartResult {
            relay_port,
            watch_urls: view::watch_urls(relay_url.as_deref(), relay_port),
            stream_id: cfg.stream_id.clone(),
        })
    }

    /// 确保 `cfg.stream_id` 是内核已签发会话（受控中继只接受内核会话 id，
    /// 需求 F2.2 / D4：id 与 stream_id 合一）。
    ///
    /// 新 UI 应先 `create_session` 取回内核签发的 id 再推流；旧 UI 直接传
    /// 自定义 id 时，在此兜底自动创建本机会话并改写 `cfg.stream_id`。
    ///
    /// **凭证推流（B1/B2）特例**：出示 `share_token` 推往远程接收端受控中继
    /// 时，`stream_id` 必须是接收端签发的会话 id——本机内核无此会话，兜底
    /// 改写会把 id 换成新会话，接收端将收不到流。因此凭证推流一律跳过。
    fn ensure_session(&self, cfg: &mut StreamConfig) -> Result<()> {
        if cfg.share_token.is_some() {
            return Ok(());
        }
        if !self.has_data_plane() || self.has_session(&cfg.stream_id) {
            return Ok(());
        }
        tracing::info!(
            "stream_id {} 未关联内核会话，自动创建本机会话",
            cfg.stream_id
        );
        let session = self.create_session(
            "local",
            &["local".into()],
            &SessionPrefs {
                title: cfg.title.clone(),
                ..Default::default()
            },
        )?;
        cfg.stream_id = session.id;
        Ok(())
    }

    /// 停止全部推流（CLI/控制面「停止推流」语义）。逐一取出引擎优雅停流。
    pub async fn stop_stream(&self) -> Result<()> {
        let streams: Vec<RunningStream> = {
            let mut g = self.engines.lock_poisoned();
            g.drain().map(|(_, s)| s).collect()
        };
        for stream in streams {
            tokio::spawn(async move {
                stream.engine.stop().await;
            });
        }
        Ok(())
    }

    /// 推流状态（并发流时报告第一条流的运行态；CLI/控制面为单流语义）。
    pub fn stream_status(&self) -> StreamStatus {
        let guard = self.engines.lock_poisoned();
        match guard.values().next() {
            Some(s) => StreamStatus {
                running: true,
                stream_id: Some(s.stream_id.clone()),
                title: Some(s.title.clone()),
                relay_port: Some(s.relay_port),
                started_at: Some(s.started_at),
            },
            None => StreamStatus {
                running: false,
                stream_id: None,
                title: None,
                relay_port: None,
                started_at: None,
            },
        }
    }

    /// 采集真实状态（Android 由原生控制帧异步回报；桌面在启动后即为就绪）。
    /// 并发流时报告第一条流的采集态。
    pub fn capture_status(&self) -> CaptureStatusView {
        let guard = self.engines.lock_poisoned();
        let active = !guard.is_empty();
        let (started, error) = match guard.values().next() {
            Some(s) => {
                let st = s.engine.capture_status();
                (st.started, st.error)
            }
            None => (false, None),
        };
        CaptureStatusView {
            active,
            started,
            error,
        }
    }

    /// 运行中推流的中继端口（供"打开观看端"使用；并发流时取第一条流）。
    pub fn stream_relay_port(&self) -> u16 {
        self.engines
            .lock_poisoned()
            .values()
            .next()
            .map_or(DEFAULT_PORT, |s| s.relay_port)
    }

    /// 本机主中继端口（`start_relay` / `start_relay_on` 启动的那个）。
    pub fn relay_port(&self) -> Option<u16> {
        self.anchor.lock_poisoned().as_ref().map(|a| a.port)
    }

    /// 本机中继全部监听端口：`(ws, srt, quic)`（未启动时为 `None`）。
    ///
    /// 防火墙自动放行按实际端口生成规则（SRT/QUIC 固定端口被占用回退随机时
    /// 也能放行真实端口）。
    pub fn relay_ports(&self) -> Option<(u16, Option<u16>, Option<u16>)> {
        self.anchor
            .lock_poisoned()
            .as_ref()
            .map(|a| (a.port, a.handle.srt_port, a.handle.quic_port))
    }

    /// 统一发现清单（`/api/discovery` 数据源，见 [`crate::discovery::DiscoveryResp`]）：
    /// 从当前锚定中继 + 身份 + 能力组装。未锚定（无中继入口）返回 `None`（非可发现节点）。
    /// `name` 用身份名，与 mDNS 广播的展示名一致（mDNS 与子网扫描都指向同一节点）。
    ///
    /// **可被发现门控**：`discoverable == false` 时也返回 `None`——「可被发现」是
    /// 隐私开关，关闭时**所有**发现路径（mDNS 广播 + 子网单播扫描回退）都不可见。
    /// 子网回退主动探测 `18779/api/discovery`，若不此处门控，mDNS 关闭仍会被
    /// 扫描发现，违背隐私优先语义（用户反馈 bug）。
    pub fn discovery_manifest(&self) -> Option<crate::discovery::DiscoveryResp> {
        // 可被发现关闭 → 不对外提供发现清单（含子网单播回退的探测口径）
        if !self.discoverable() {
            return None;
        }
        let (relay_port, srt_port, quic_port) = self.relay_ports()?;
        let identity = self.device_identity()?;
        let info = self.mdns_info(&identity.device_name);
        Some(crate::discovery::DiscoveryResp {
            device_id: identity.device_id,
            name: info.name,
            relay_port,
            srt_port,
            quic_port,
            roles: info.roles,
            media: info.media,
            transports: info.transports,
            endpoints: info.endpoints,
        })
    }

    /// 实例已运行秒数（控制面 Status 展示 uptime）。
    pub fn uptime_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    // -----------------------------------------------------------------------
    // 接收播放（1e）
    // -----------------------------------------------------------------------

    /// 开始接收 `relay_url` 上的 `stream_id`（WS watch → 抖动缓冲 → 原生解码）。
    ///
    /// 返回的 [`Receiver`] 解码帧通道经 [`Kernel::take_receive_frames`]
    /// 交给上层（GUI 绘制）；同时只允许一个接收会话。`audio_out` 决定音频去向
    /// （设备播放 / 丢弃）。Android 请用 [`Kernel::start_receive_raw`]
    /// （编码帧 → Kotlin MediaCodec）。
    #[cfg(not(target_os = "android"))]
    pub async fn start_receive(
        &self,
        relay_url: String,
        stream_id: String,
        audio_out: AudioOut,
    ) -> Result<Arc<Receiver>> {
        {
            let guard = self.receiver.lock_poisoned();
            if let Some(r) = guard.as_ref() {
                r.stop(); // 先停旧的
            }
        }
        let r = Receiver::start(relay_url, stream_id, audio_out, self.local_proxy()).await?;
        *self.receiver.lock_poisoned() = Some(r.clone());
        Ok(r)
    }

    /// 开始接收 `relay_url` 上的 `stream_id`（WS watch → 抖动缓冲 → **不解码**）。
    ///
    /// 编码帧经 [`Kernel::take_receive_raw_frames`] 交给上层（Android 播放：
    /// Kotlin MediaCodec 解码）；同时只允许一个接收会话。
    pub async fn start_receive_raw(
        &self,
        relay_url: String,
        stream_id: String,
    ) -> Result<Arc<Receiver>> {
        {
            let guard = self.receiver.lock_poisoned();
            if let Some(r) = guard.as_ref() {
                r.stop(); // 先停旧的
            }
        }
        let r = Receiver::start_raw(relay_url, stream_id, self.local_proxy()).await?;
        *self.receiver.lock_poisoned() = Some(r.clone());
        Ok(r)
    }

    /// 停止接收。
    pub fn stop_receive(&self) {
        if let Some(r) = self.receiver.lock_poisoned().take() {
            r.stop();
        }
    }

    /// 本机中继的代理能力（观看直连失败时级联兜底）；本机中继未启动时为 `None`。
    fn local_proxy(&self) -> Option<LocalProxy> {
        self.anchor.lock_poisoned().as_ref().map(|a| LocalProxy {
            state: a.handle.state(),
            ws_base: crate::transport::RelayUrl::ws("127.0.0.1", a.port, None).to_string(),
        })
    }

    /// 取出当前接收会话的解码帧通道（每会话一次）。
    pub fn take_receive_frames(&self) -> Option<mpsc::Receiver<RenderedFrame>> {
        self.receiver
            .lock_poisoned()
            .as_ref()
            .and_then(|r| r.take_frames())
    }

    /// 取出当前接收会话的编码帧通道（`start_receive_raw`；每会话一次）。
    pub fn take_receive_raw_frames(&self) -> Option<mpsc::Receiver<Frame>> {
        self.receiver
            .lock_poisoned()
            .as_ref()
            .and_then(|r| r.take_raw_frames())
    }

    /// 当前接收统计。
    pub fn receive_status(&self) -> crate::receiver::ReceiveStats {
        self.receiver
            .lock_poisoned()
            .as_ref()
            .map(|r| r.stats())
            .unwrap_or_default()
    }

    /// Android 播放路径回写：Kotlin `PlaybackPlugin` 每解码一帧回调一次。
    pub fn note_android_decoded_frame(&self) {
        if let Some(r) = self.receiver.lock_poisoned().as_ref() {
            r.note_decoded_video();
        }
    }

    // -----------------------------------------------------------------------
    // 跨设备凭证（B2：接收手机麦克风）
    // -----------------------------------------------------------------------

    /// 接收端入口：建本机会话 → 签发一次性接入凭证（默认 10 分钟）。
    ///
    /// 手机出示凭证直接推入本机受控中继（`Hello.share_token`），电脑随后
    /// 用同一会话 id 原生接收——B0 凭证式接入，不开放任何远程控制面。
    pub fn issue_share_token(&self, ttl_secs: Option<u64>) -> Result<stross_types::ShareTokenView> {
        self.issue_share_token_for("接收手机麦克风".into(), vec![MediaKind::Mic], ttl_secs)
    }

    /// 通用凭证签发（媒体 / 标题可定制；协商端点与手动路径共用）。
    pub fn issue_share_token_for(
        &self,
        title: String,
        media: Vec<MediaKind>,
        ttl_secs: Option<u64>,
    ) -> Result<stross_types::ShareTokenView> {
        let prefs = SessionPrefs {
            title,
            ..Default::default()
        };
        let session = self.create_session("local", &["local".into()], &prefs)?;
        let ttl = Duration::from_secs(ttl_secs.unwrap_or(600));
        let token = self.create_share_token(&session.id, media, ttl)?;
        Ok(stross_types::ShareTokenView {
            token: token.to_token_string(),
            stream_id: token.stream_id,
            pin: token.pin,
            expires_at: token.expires_at,
        })
    }
}

/// 端点注入目标（stross-endpoint 端点装配用）：登记 + 平台查询。
impl stross_endpoint::factory::EndpointSeeder for Kernel {
    fn seed_endpoint(&self, ep: Box<dyn Endpoint>) -> bool {
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

    fn note_share_active(
        &self,
        self_weak: std::sync::Weak<dyn EndpointApp>,
        endpoint_id: &str,
        stream_id: &str,
        delivery: Delivery,
    ) {
        Self::note_share_active(self, self_weak, endpoint_id, stream_id, delivery);
    }

    fn stop_share_if_unwatched(&self, stream_id: &str) {
        Self::stop_share_if_unwatched(self, stream_id);
    }
}

/// 数据面接入凭证校验器：读内核签发表，校验"存在 + 未过期 + 逐字一致"。
struct KernelTokenValidator {
    tokens: Arc<Mutex<HashMap<String, ShareToken>>>,
}

impl crate::relay::ShareTokenValidator for KernelTokenValidator {
    fn validate(&self, token: &ShareToken) -> bool {
        let tokens = self.tokens.lock_poisoned();
        let Some(stored) = tokens.get(&token.stream_id) else {
            return false;
        };
        stored == token && !stored.is_expired(now_secs())
    }
}

/// 本机中继的 mDNS 实例名：携带持久化 `device_id` 前 8 位 + 端口，保证
/// 局域网内多设备同端口广播时实例名唯一（mdns-sd browse 按实例名键控，
/// 同名实例会互相覆盖导致扫描不到，实测）。
///
/// 未注入身份时回退旧格式 `sender-{port}`（兼容无 UI 接入方）。
fn relay_mdns_instance(device_id: Option<&str>, port: u16) -> String {
    match device_id {
        Some(id) if !id.is_empty() => {
            let short = id.chars().take(8).collect::<String>();
            format!("stross-{short}-{port}")
        }
        _ => format!("sender-{port}"),
    }
}

/// 当前 Unix 秒（公共实现见 [`stross_proto::time`]）。
fn now_secs() -> u64 {
    stross_proto::time::unix_secs()
}

/// 一次性凭证 PIN（6 位数字）。
///
/// 非密码学随机（一次性凭证防误连/旁观冒用即可）：`DefaultHasher` 每次运行
/// 带进程随机种子，混合会话 id 与纳秒时间，碰撞概率可忽略；不引入 rand 依赖。
fn random_pin(seed: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    seed.hash(&mut h);
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut h);
    let v = h.finish();
    format!("{:06}", v % 1_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;
    use stross_proto::message::ReliabilityProfile;

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
    impl CaptureBackend for MockBackend {
        fn start(&self, _cfg: &StreamConfig, _tx: mpsc::Sender<Frame>) -> anyhow::Result<()> {
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
        use stross_proto::message::{CapabilityKind, MediaKind};
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
        use stross_proto::message::MediaKind;
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
        k.note_share_active(weak, "mic:builtin", "sess-1", Delivery::Pull);
        let got = k
            .active_share_by_endpoint("mic:builtin")
            .expect("登记后可查询");
        assert_eq!(got.0, "sess-1");
        assert_eq!(got.1, Delivery::Pull);

        k.stop_endpoint_share("mic:builtin").unwrap();
        assert!(
            k.active_share_by_endpoint("mic:builtin").is_none(),
            "停止后登记应清除"
        );
        // 幂等：无活动共享时停止直接成功
        assert!(k.stop_endpoint_share("mic:builtin").is_ok());
        assert!(k.stop_endpoint_share("screen:0").is_ok());
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
}
