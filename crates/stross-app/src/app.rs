//! 核心封装模块：应用级状态机与命令面。
//!
//! [`StrossApp`] 把共享模块（中继/推流客户端/发现）与系统适配模块（采集后端）
//! 组合成"先连接、再推流/观看"的完整应用逻辑，**不依赖任何 UI 框架**：
//!
//! * 无 Tauri / web 依赖，可在纯 Rust 环境下单元测试
//! * UI 层（桌面 / Android）只负责把命令转发到这里
//! * 采集后端通过 [`Arc<dyn CaptureBackend>`] 注入，平台差异被隔离在
//!   `stross-media`（桌面 ffmpeg）与 UI 层（Android 原生）实现里

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use stross_core::discovery::Discovery;
use stross_core::net::local_ips;
use stross_core::relay::{DEFAULT_PORT, RelayHandle, RelayServer};
use stross_media::capture::CaptureBackend;
use stross_media::pipeline::{StreamConfig, ffmpeg_available};
#[cfg(not(target_os = "android"))]
use stross_media::playback::AudioOut;
use stross_media::playback::RenderedFrame;
use tokio::sync::mpsc;

use crate::receiver::{LocalProxy, ReceiveStats, Receiver};
use stross_proto::frame::Frame;
use stross_proto::message::{
    CodecId, Delivery, DeviceInfo, DeviceSummary, DiscoveryInfo, EndpointManifest, MediaKind,
    RoleId, TransportId, TransportPreference, Visibility,
};

use crate::engine::SenderEngine;
use crate::error::{Error, Result};

use crate::kernel::{EndpointRegistry, Kernel, NodeInfo, NodeRole, RelayDataPlane, SessionPrefs};
use crate::lock::MutexExt;

/// 运行平台（UI 层注入）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Desktop,
    Android,
}

impl Platform {
    pub fn as_str(&self) -> &'static str {
        match self {
            Platform::Desktop => "desktop",
            Platform::Android => "android",
        }
    }
}

/// P1 平台设备静态枚举（docs/endpoint-model.md §11：camera 按采集能力后置）。
///
/// 设备 = 持久能力实体（摄像头 / 麦克风 / 屏幕），与订阅与否无关；被公开后
/// 才实例化为端点。Android P1 不默认公开屏幕（依赖前台服务权限，micOnly 路径
/// 已验证，屏幕采集权限后置）。
fn platform_devices(platform: Platform) -> Vec<DeviceInfo> {
    let mut v = vec![
        DeviceInfo {
            device_id: "screen:0".into(),
            kind: MediaKind::Screen,
            name: "屏幕".into(),
            builtin: true,
        },
        DeviceInfo {
            device_id: "mic:builtin".into(),
            kind: MediaKind::Mic,
            name: "麦克风".into(),
            builtin: true,
        },
        DeviceInfo {
            device_id: "sysaudio:builtin".into(),
            kind: MediaKind::SystemAudio,
            name: "系统声音".into(),
            builtin: true,
        },
    ];
    if matches!(platform, Platform::Android) {
        v.retain(|d| d.kind != MediaKind::Screen);
    }
    v
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

/// 应用全局状态。
pub struct StrossApp {
    platform: Platform,
    engine: Mutex<Option<RunningStream>>,
    /// 本机锚点：常驻受控中继 + mDNS 广播（免先连：一起启动、生命周期一致）。
    anchor: Mutex<Option<LocalAnchor>>,
    /// 采集后端（平台相关，UI 层注入；`Arc` 使其可被引擎复用）。
    backend: Mutex<Option<Arc<dyn CaptureBackend>>>,
    /// 内核（控制面）：设备图 / 会话管理 / 路由（设计文档 §3）。
    kernel: Kernel,
    /// 端点框架（docs/endpoint-model.md）：节点设备表 + 已公开端点表（P1 1:1）。
    registry: Mutex<EndpointRegistry>,
    /// 接收播放（1e）：WS 收流 → SessionDataManager → PlaybackSink 解码。
    receiver: Mutex<Option<Arc<Receiver>>>,
    /// 本机持久化身份（UI 层 `load_or_create_identity` 注入；用于 mDNS
    /// 实例名唯一化——多设备同端口广播不再同名串扰）。
    identity: Mutex<Option<crate::negotiator::DeviceIdentity>>,
    /// 实例启动时刻（控制面 Status 的 uptime 统计源）。
    started: std::time::Instant,
}

/// 本机锚点（免先连：应用打开即自动建立；推流 / 观看 / 局域网发现共用）。
struct LocalAnchor {
    handle: RelayHandle,
    /// mDNS 广播句柄（启动失败时为 `None`：中继仍可用，仅局域网不可发现）。
    /// 仅用于持有：drop 即停止广播（RAII），无需读取。
    #[allow(dead_code)]
    discovery: Option<Discovery>,
    /// 中继实际监听端口（绑定 0 自动分配时取实际值）。
    port: u16,
}

/// 运行中的推流。
struct RunningStream {
    engine: SenderEngine,
    relay_port: u16,
    title: String,
    stream_id: String,
    started_at: u64,
}

impl StrossApp {
    pub fn new(platform: Platform) -> Self {
        Self {
            platform,
            engine: Mutex::new(None),
            anchor: Mutex::new(None),
            backend: Mutex::new(None),
            kernel: Kernel::new(),
            registry: Mutex::new(EndpointRegistry::with_devices(platform_devices(platform))),
            receiver: Mutex::new(None),
            identity: Mutex::new(None),
            started: std::time::Instant::now(),
        }
    }

    /// 内核（控制面）引用。
    pub fn kernel(&self) -> &Kernel {
        &self.kernel
    }

    /// 注入采集后端（UI 层在启动时调用一次）。
    pub fn set_backend(&self, backend: Arc<dyn CaptureBackend>) {
        *self.backend.lock_poisoned() = Some(backend);
    }

    /// 注入本机持久化身份（UI 层启动时调用；缺失时 mDNS 实例名回退旧格式）。
    pub fn set_identity(&self, id: crate::negotiator::DeviceIdentity) {
        *self.identity.lock_poisoned() = Some(id);
    }

    // -----------------------------------------------------------------------
    // 信息与设备
    // -----------------------------------------------------------------------

    /// 应用信息（版本 / 平台 / ffmpeg 是否可用 / 本机 IP）。
    pub fn app_info(&self) -> AppInfo {
        AppInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            platform: self.platform.as_str().to_string(),
            ffmpeg: ffmpeg_available(),
            ips: local_ips().into_iter().map(|ip| ip.to_string()).collect(),
        }
    }

    /// 运行平台字符串（"desktop" / "android"；控制面 Status 展示）。
    pub fn platform_str(&self) -> &'static str {
        self.platform.as_str()
    }

    /// 摄像头 / 麦克风 / 系统声音设备列表。
    pub fn list_devices(&self) -> DeviceList {
        DeviceList {
            cameras: stross_media::devices::list_cameras(),
            audio_inputs: stross_media::devices::list_audio_inputs(),
            system_audio: stross_media::devices::list_system_audio(),
        }
    }

    // -----------------------------------------------------------------------
    // 端点框架（P1：节点 → 设备 → 端点，见 docs/endpoint-model.md）
    // -----------------------------------------------------------------------

    /// 公开设备为端点（P1 1:1：同一设备重复公开报错）。
    ///
    /// `transports` / `codecs` 缺省时按端点类型给默认（协议由公开者声明：
    /// 媒体类 Lossy → QUIC>SRT>WS，其余 Lossless → QUIC>WS）。
    pub fn publish_endpoint(
        &self,
        device_id: &str,
        visibility: Visibility,
        delivery: Delivery,
        transports: Option<Vec<TransportPreference>>,
        codecs: Option<Vec<CodecId>>,
    ) -> Result<EndpointManifest> {
        let mut reg = self.registry.lock_poisoned();
        let kind = reg
            .device(device_id)
            .ok_or_else(|| Error::Message(format!("设备不存在: {device_id}")))?
            .kind;
        let transports = transports.unwrap_or_else(|| EndpointRegistry::default_transports(kind));
        let codecs = codecs.unwrap_or_else(|| vec![CodecId::H264, CodecId::Aac]);
        reg.publish(device_id, visibility, delivery, transports, codecs)
    }

    /// 取消公开端点（已订阅会话由上层决定宽限期，P1 直接移除）。
    pub fn unpublish_endpoint(&self, endpoint_id: &str) -> Result<()> {
        self.registry.lock_poisoned().unpublish(endpoint_id)
    }

    /// 公开本地文件为文件端点（动态设备 `file:<名>`；本地路径登记但不出现在
    /// 目录 / 摘要 / wire，见 docs/endpoint-model.md §3.6）。
    pub fn publish_file_endpoint(
        &self,
        path: &std::path::Path,
        visibility: Visibility,
        delivery: Delivery,
    ) -> Result<EndpointManifest> {
        self.registry
            .lock_poisoned()
            .publish_file(path, visibility, delivery)
    }

    /// 文件端点的本地文件源（订阅驱动开推用）。
    pub fn file_source(&self, endpoint_id: &str) -> Option<crate::kernel::FileSource> {
        self.registry
            .lock_poisoned()
            .file_source(endpoint_id)
            .cloned()
    }

    /// 接线订阅事件回调（上层启动时安装一次；协商层授予成功后触发，
    /// 见 docs/endpoint-model.md §5 联动）。
    pub fn set_subscribe_hook(&self, hook: Option<Arc<crate::kernel::SubscribeHook>>) {
        self.registry.lock_poisoned().set_subscribe_hook(hook);
    }

    /// 订阅达成事件（协商层授予成功后调用；触发上层驱动开推）。
    ///
    /// hook 在注册表锁**外**调用：驱动回调会再次访问注册表
    /// （`endpoint_manifest` / `file_source`），持锁回调会死锁（实测挂死握手）。
    pub fn fire_endpoint_subscribed(&self, endpoint_id: &str, ctx: &crate::kernel::SubscribeCtx) {
        let hook = self.registry.lock_poisoned().subscribed_hook();
        if let Some(hook) = hook {
            hook(endpoint_id, ctx);
        }
    }

    /// 端点清单查询（订阅握手 / 目录 API 用）。
    pub fn endpoint_manifest(&self, endpoint_id: &str) -> Option<EndpointManifest> {
        self.registry.lock_poisoned().manifest(endpoint_id).cloned()
    }

    /// 目录快照：全部设备 + 全部端点（Private 端点可见性过滤由调用方做）。
    pub fn endpoint_catalog(&self) -> (Vec<DeviceInfo>, Vec<EndpointManifest>) {
        let reg = self.registry.lock_poisoned();
        (reg.devices(), reg.manifests())
    }

    /// 本机持久化身份（目录 API 的 node 信息源）。
    pub fn device_identity(&self) -> Option<crate::negotiator::DeviceIdentity> {
        self.identity.lock_poisoned().clone()
    }

    // -----------------------------------------------------------------------
    // 连接阶段
    // -----------------------------------------------------------------------

    /// 启动/复用本机中继（"先连接"步骤的本机选项）。
    ///
    /// 本机中继以**受控模式**启动（需求 F2.2「先会话后传输」）：只有内核
    /// 创建的会话 id 才能推流；中继作为数据面后端接入内核
    /// （[`Kernel::attach_data_plane`]），流生命周期事件转发为 [`KernelEvent`]。
    pub async fn start_relay(&self) -> Result<RelayInfo> {
        self.start_relay_on(DEFAULT_PORT).await
    }

    /// 在指定端口启动中继（受内核控制的数据面）；被占用时回退随机端口。
    pub async fn start_relay_on(&self, port: u16) -> Result<RelayInfo> {
        self.start_relay_fixed(port, 0, 0).await
    }

    /// 在指定端口启动中继，并固定 SRT/QUIC 传输端口（`0` = 随机）。
    ///
    /// 固定端口便于防火墙只放行已知端口（权限自动化）；SRT/QUIC 被占用时
    /// 各自回退随机端口（实际端口经 `scan_relays` / `/api/info` 可见）。
    pub async fn start_relay_fixed(
        &self,
        port: u16,
        srt_port: u16,
        quic_port: u16,
    ) -> Result<RelayInfo> {
        {
            let guard = self.anchor.lock_poisoned();
            if let Some(a) = guard.as_ref() {
                return Ok(relay_info(
                    a.port,
                    self.registry.lock_poisoned().summaries(),
                ));
            }
        }
        // 优先指定端口；被占用时回退随机端口（本机中继"能用就行"，不因端口冲突失败）
        let handle = match RelayServer::start_controlled_with(port, srt_port, quic_port).await {
            Ok(h) => h,
            Err(_) => {
                tracing::warn!("端口 {port} 被占用，本机中继回退到随机端口");
                RelayServer::start_controlled(0).await?
            }
        };
        let port = handle.port;
        // 中继接入内核（数据面后端）：订阅流事件、会话预授权
        self.kernel
            .attach_data_plane(Arc::new(RelayDataPlane::new(&handle)));
        // 把本机注册进内核设备图（含采集能力，供会话协商）
        self.register_local_node();
        // mDNS 广播本机中继，局域网内其它设备（如电脑端 Stross）可扫描发现。
        // 能力描述统一走 DiscoveryInfo 单 key JSON（F1.2 / 1d）。
        // 多网卡：广播全部局域网 IP（Discovery::start 内部处理空列表回退回环），
        // 避免只广播第一个 IP 导致其它网卡网段扫描不到本机
        let discovery = {
            // mDNS 实例名唯一化：同名实例（LAN 内多设备同为 8777 端口 →
            // 旧 `sender-8777`）会被 mdns-sd browse 按键控互覆盖，导致
            // 扫不到对方（实测）。实例名携带持久化 device_id（前 8 位）
            // + 端口，任何设备同端口广播也不碰撞；未注入身份时回退旧格式。
            let instance = relay_mdns_instance(
                self.identity
                    .lock_poisoned()
                    .as_ref()
                    .map(|id| id.device_id.as_str()),
                port,
            );
            let info = DiscoveryInfo::relay_default(
                "Stross 本机中继",
                vec![
                    MediaKind::Screen,
                    MediaKind::Camera,
                    MediaKind::Mic,
                    MediaKind::SystemAudio,
                ],
            )
            .with_devices(self.registry.lock_poisoned().summaries());
            match Discovery::start(&instance, &local_ips(), port, &info) {
                Ok(d) => Some(d),
                Err(e) => {
                    tracing::warn!("mDNS 广播失败: {e}");
                    None
                }
            }
        };
        *self.anchor.lock_poisoned() = Some(LocalAnchor {
            handle,
            discovery,
            port,
        });
        Ok(relay_info(port, self.registry.lock_poisoned().summaries()))
    }

    /// 把本机节点（含采集能力）注册进内核设备图。
    pub fn register_local_node(&self) {
        let kernel = &self.kernel;
        kernel.upsert_node(NodeInfo {
            node_id: "local".into(),
            name: hostname::get()
                .map(|h| h.to_string_lossy().to_string())
                .unwrap_or_else(|_| "本机".into()),
            roles: vec![NodeRole::Sender, NodeRole::Viewer, NodeRole::Relay],
            caps: vec![],
            addrs: vec![],
        });
        if let Some(backend) = self.backend.lock_poisoned().as_ref() {
            kernel.register_capability("local", backend.descriptor());
        }
    }

    /// mDNS 扫描局域网内的其它中继。
    ///
    /// 返回的 [`RelayInfo`] 透传 mDNS 能力引导信息（设备名 / 角色 / 传输），
    /// 供前端直接展示设备卡片，无需再手动输入地址。
    pub async fn scan_relays(&self) -> Result<Vec<RelayInfo>> {
        let found = Discovery::browse(stross_core::discovery::BROWSE_TIMEOUT).await?;
        tracing::debug!("scan_relays 发现 {} 台设备", found.len());
        Ok(found
            .into_iter()
            .map(|d| {
                // 单 key JSON 解码（F1.2）；旧设备 / 缺失时回退默认值
                let info = DiscoveryInfo::from_txt(&d.txt);
                let url = stross_core::transport::RelayUrl::http(&d.ip.to_string(), d.port);
                RelayInfo {
                    port: d.port,
                    urls: vec![url],
                    name: info.as_ref().map(|i| i.name.clone()),
                    kind: info.as_ref().map(|_| "relay".into()),
                    roles: info.as_ref().map(|i| i.roles.clone()).unwrap_or_default(),
                    transports: info
                        .as_ref()
                        .map(|i| i.transports.clone())
                        .unwrap_or_default(),
                    ip: Some(d.ip.to_string()),
                    devices: info.as_ref().map(|i| i.devices.clone()).unwrap_or_default(),
                }
            })
            .collect())
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
        if self.engine.lock_poisoned().is_some() {
            return Err(Error::Message("已经在推流中，请先停止".into()));
        }
        let backend = self
            .backend
            .lock_poisoned()
            .clone()
            .ok_or_else(|| Error::Message("采集后端未初始化".into()))?;
        // 会话兜底：受控中继只接受内核会话 id；未建会话时自动创建
        self.ensure_session(&mut cfg)?;
        // 未指定中继时，推到已连接（常驻）的本机中继；
        // 推流地址按媒体类型自动选传输（视频→SRT>QUIC>WS，纯音频→QUIC>WS）
        let relay_url = match relay_url {
            Some(u) => Some(u),
            None => {
                let guard = self.anchor.lock_poisoned();
                guard
                    .as_ref()
                    .map(|a| a.handle.auto_push_url(cfg.video.is_some()))
            }
        };
        let engine =
            SenderEngine::start(cfg.clone(), backend, relay_url.clone(), DEFAULT_PORT).await?;
        // 有效中继端口：内嵌中继 > 常驻中继 > 默认端口
        let relay_port = engine
            .relay_port()
            .or_else(|| self.anchor.lock_poisoned().as_ref().map(|a| a.port))
            .unwrap_or(DEFAULT_PORT);
        let started_at = stross_proto::time::unix_secs();
        *self.engine.lock_poisoned() = Some(RunningStream {
            engine,
            relay_port,
            title: cfg.title.clone(),
            stream_id: cfg.stream_id.clone(),
            started_at,
        });
        Ok(StartResult {
            relay_port,
            watch_urls: watch_urls(relay_url.as_deref(), relay_port),
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
        if !self.kernel.has_data_plane() || self.kernel.has_session(&cfg.stream_id) {
            return Ok(());
        }
        tracing::info!(
            "stream_id {} 未关联内核会话，自动创建本机会话",
            cfg.stream_id
        );
        let session = self.kernel.create_session(
            "local",
            &["local".into()],
            &crate::SessionPrefs {
                title: cfg.title.clone(),
                ..Default::default()
            },
        )?;
        cfg.stream_id = session.id;
        Ok(())
    }

    /// 停止推流。
    pub async fn stop_stream(&self) -> Result<()> {
        let engine = self.engine.lock_poisoned().take();
        if let Some(stream) = engine {
            tokio::spawn(async move {
                stream.engine.stop().await;
            });
        }
        Ok(())
    }

    /// 推流状态。
    pub fn stream_status(&self) -> StreamStatus {
        let guard = self.engine.lock_poisoned();
        match guard.as_ref() {
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
    pub fn capture_status(&self) -> CaptureStatusView {
        let guard = self.engine.lock_poisoned();
        let active = guard.is_some();
        let (started, error) = match guard.as_ref() {
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

    /// 运行中推流的中继端口（供"打开观看端"使用）。
    pub fn stream_relay_port(&self) -> u16 {
        self.engine
            .lock_poisoned()
            .as_ref()
            .map(|s| s.relay_port)
            .unwrap_or(DEFAULT_PORT)
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

    /// 实例已运行秒数（控制面 Status 展示 uptime）。
    pub fn uptime_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    // -----------------------------------------------------------------------
    // 接收播放（1e）
    // -----------------------------------------------------------------------

    /// 开始接收 `relay_url` 上的 `stream_id`（WS watch → 抖动缓冲 → 原生解码）。
    ///
    /// 返回的 [`Receiver`] 解码帧通道经 [`StrossApp::take_receive_frames`]
    /// 交给上层（GUI 绘制）；同时只允许一个接收会话。`audio_out` 决定音频去向
    /// （设备播放 / 丢弃）。Android 请用 [`StrossApp::start_receive_raw`]
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
    /// 编码帧经 [`StrossApp::take_receive_raw_frames`] 交给上层（Android 播放：
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

    // -----------------------------------------------------------------------
    // 跨设备凭证（B2：接收手机麦克风）
    // -----------------------------------------------------------------------

    /// 接收端入口：建本机会话 → 签发一次性接入凭证（默认 10 分钟）。
    ///
    /// 手机出示凭证直接推入本机受控中继（`Hello.share_token`），电脑随后
    /// 用同一会话 id 原生接收——B0 凭证式接入，不开放任何远程控制面。
    pub fn issue_share_token(&self, ttl_secs: Option<u64>) -> Result<ShareTokenView> {
        self.issue_share_token_for("接收手机麦克风".into(), vec![MediaKind::Mic], ttl_secs)
    }

    /// 通用凭证签发（媒体 / 标题可定制；协商端点与手动路径共用）。
    pub fn issue_share_token_for(
        &self,
        title: String,
        media: Vec<MediaKind>,
        ttl_secs: Option<u64>,
    ) -> Result<ShareTokenView> {
        let prefs = SessionPrefs {
            title,
            ..Default::default()
        };
        let session = self
            .kernel()
            .create_session("local", &["local".into()], &prefs)?;
        let ttl = Duration::from_secs(ttl_secs.unwrap_or(600));
        let token = self.kernel().create_share_token(&session.id, media, ttl)?;
        Ok(ShareTokenView {
            token: token.to_token_string(),
            stream_id: token.stream_id,
            pin: token.pin,
            expires_at: token.expires_at,
        })
    }

    /// 本机中继的代理能力（观看直连失败时级联兜底）；本机中继未启动时为 `None`。
    fn local_proxy(&self) -> Option<LocalProxy> {
        use stross_core::transport::RelayUrl;
        self.anchor.lock_poisoned().as_ref().map(|a| LocalProxy {
            state: a.handle.state(),
            ws_base: RelayUrl::ws("127.0.0.1", a.port, None).to_string(),
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
    pub fn receive_status(&self) -> ReceiveStats {
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
}

// ---------------------------------------------------------------------------
// 值类型
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub version: String,
    /// "desktop" | "android"
    pub platform: String,
    pub ffmpeg: bool,
    pub ips: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceList {
    pub cameras: Vec<stross_media::devices::CameraDevice>,
    pub audio_inputs: Vec<String>,
    pub system_audio: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayInfo {
    pub port: u16,
    pub urls: Vec<String>,
    /// 设备名（mDNS 能力引导 `name`；本机中继或缺失时为 `None`）。
    pub name: Option<String>,
    /// 类型（relay / sender / …）。
    pub kind: Option<String>,
    /// 角色（mDNS 能力引导 `roles`；枚举，序列化与字符串时代一致）。
    pub roles: Vec<RoleId>,
    /// 支持的传输（mDNS 能力引导 `transports`；序列化后与字符串时代一致）。
    pub transports: Vec<TransportId>,
    /// 中继 IP（本机中继时为 `None`，用 urls 展示）。
    pub ip: Option<String>,
    /// 端点框架 L1：该节点公开的设备清单摘要（id/kind/name/是否已公开；
    /// 本机 = 注册表快照，对端 = mDNS `DiscoveryInfo v2.devices` 解码）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<DeviceSummary>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartResult {
    pub relay_port: u16,
    pub watch_urls: Vec<String>,
    /// 实际流 id（内核签发，D4：与 session id 合一；接收端据此订阅）。
    pub stream_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamStatus {
    pub running: bool,
    pub stream_id: Option<String>,
    pub title: Option<String>,
    pub relay_port: Option<u16>,
    pub started_at: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureStatusView {
    pub active: bool,
    pub started: bool,
    pub error: Option<String>,
}

/// 手机麦克风接入凭证视图（B2：电脑端签发后展示给手机）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareTokenView {
    /// ShareToken JSON 字符串（手机端原样粘贴到「共享麦克风」）。
    pub token: String,
    /// 接收端签发的会话 id（= 接收时的流 id）。
    pub stream_id: String,
    /// 一次性 PIN（展示用；服务端签发表为准）。
    pub pin: String,
    /// 过期时间（Unix 秒）。
    pub expires_at: u64,
}

fn relay_info(port: u16, devices: Vec<DeviceSummary>) -> RelayInfo {
    // 多网卡：列出全部局域网 IP 入口（无局域网 IP 时回退回环）
    let urls = stross_core::transport::RelayUrl::http_entries(port);
    RelayInfo {
        port,
        urls,
        name: Some("Stross 本机中继".into()),
        kind: Some("relay".into()),
        roles: vec![RoleId::Sender, RoleId::Viewer, RoleId::Relay],
        transports: vec![
            TransportId::Ws,
            TransportId::WebRtc,
            TransportId::Srt,
            TransportId::Quic,
        ],
        ip: None,
        devices,
    }
}

/// 局域网可访问的中继入口（供其它设备连接数据面 / REST 端点）。
///
/// * 推到外部中继（`relay_url` 非回环地址）→ 直接指向该中继
/// * 本机中继（回环地址 / 未指定）→ 列出本机局域网 IP
fn watch_urls(relay_url: Option<&str>, relay_port: u16) -> Vec<String> {
    if let Some(url) = relay_url.and_then(stross_core::transport::RelayUrl::parse) {
        // 仅 ws 基址可直接反推 HTTP 入口（srt/quic 属 UDP 数据面端口，入口仍是本机 HTTP）
        if url.is_ws() && !url.is_loopback() {
            return vec![url.base_http()];
        }
    }
    stross_core::transport::RelayUrl::http_entries(relay_port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use stross_proto::frame::Frame;
    use tokio::sync::mpsc;

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
        fn status(&self) -> stross_media::capture::CaptureStatus {
            stross_media::capture::CaptureStatus {
                started: self.0.load(std::sync::atomic::Ordering::SeqCst),
                error: None,
            }
        }
    }

    #[test]
    fn app_info_and_devices_never_panic() {
        let app = StrossApp::new(Platform::Desktop);
        let info = app.app_info();
        assert_eq!(info.platform, "desktop");
        let _ = app.list_devices();
    }

    #[test]
    fn capture_status_requires_backend() {
        let app = StrossApp::new(Platform::Desktop);
        // 未注入后端时采集状态应为未激活
        let st = app.capture_status();
        assert!(!st.active);
        assert!(!st.started);
    }

    #[test]
    fn set_backend_then_query() {
        let app = StrossApp::new(Platform::Android);
        app.set_backend(Arc::new(MockBackend(std::sync::atomic::AtomicBool::new(
            false,
        ))));
        let st = app.capture_status();
        assert!(!st.active); // 未推流
    }

    #[test]
    fn platform_str() {
        assert_eq!(Platform::Desktop.as_str(), "desktop");
        assert_eq!(Platform::Android.as_str(), "android");
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
}
