//! 端点 SPI（分享端 / 订阅端契约）——**内核约定特性、端点实现、内核只基于
//! 特性行动**（docs/endpoint-model-v2.md §3）。
//!
//! 本模块是**契约单一真源**（stross-types 共享契约层）：内核（stross-kernel）
//! 声明它需要什么（[`ShareEndpoint`] / [`SubscribeEndpoint`] / [`EndpointApp`]，
//! 经 `stross_kernel::*` 重导出），端点插件区（stross-endpoint）实现这些特性
//! （`stross_endpoint::contract` 重导出，路径兼容）；两端都只依赖本契约与
//! wire 层（stross-proto），互不依赖对方的具体类型。
//!
//! * [`Endpoint`]：公共视图（身份 / 内容类型 / 能力族 / 策略档案）
//! * [`ShareEndpoint`]（分享端点 = 内容源）：`load` 探测 + `share` 开推
//! * [`SubscribeEndpoint`]（订阅端点 = 内容宿）：`subscribe` 消费流并还原
//! * [`MediaSourceEndpoint`]：Graph / Audio 类分享端的族实现（只声明
//!   `video()`/`audio()`，share/策略/传输族默认）
//! * [`EndpointApp`]：端点可见的内核调度能力（内核实现）
//! * 数据契约：[`StreamConfig`] / [`VideoSource`] / [`AudioSourceConfig`] /
//!   [`Quality`] / [`FilePushOptions`] —— 分享端与订阅端之间传输的纯数据
//!   载荷（serialize + pick 的策略组合在 stross-proto）
//!
//! 目标类型（[`TargetKind`]）：确定目标（文件等，一次推送、有完成态、Lossless）
//! 与实时目标（屏幕等，持续推流、Lossy）——差异经目标类型维度 + 各端点实现表达。

use std::path::PathBuf;
use std::result::Result as StdResult;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use stross_proto::message::{
    CodecId, ControlMessage, Delivery, EndpointId, EndpointStrategy, MediaKind, PickRule,
    ReliabilityProfile, SubscribeSpec, TrackInfo,
};

/// 目标类型：端点分两类的维度（决定默认传输 Lossless/Lossy 与共享生命周期）。
///
/// **不进 wire**（`MediaKind` 已足够标识，避免冗余字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    /// 确定目标：内容在共享前已定（文件 / 剪贴板），一次推送、有完成态。
    Determined,
    /// 实时目标：内容持续产生（屏幕 / 相机 / 麦克风 / 系统声音），持续推流。
    Live,
}

/// 端点能力类别（按数据形态分族，docs/endpoint-model-v2.md §3）。
///
/// **同一族 = 同一类分享/订阅实现**：分享端（[`MediaSourceEndpoint`] 统一
/// 「组流推流」）与订阅端（订阅端点生成按族分发）都按族共享，消灭逐个端点
/// 的样板代码。**不进 wire**（由 [`MediaKind`] 推导，避免冗余字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointClass {
    /// 图形/视频源（屏幕 / 窗口 / 摄像头）：帧流，编码/渲染路径。
    Graph,
    /// 音频源（麦克风 / 系统声音）：音频流，编码/播放路径。
    Audio,
    /// 确定目标文件（分享推送 / 订阅接收落盘）。
    File,
    /// 剪贴板（文本/图像同步；预留）。
    Clipboard,
    /// 游戏输入（键鼠/手柄注入；预留）。
    Input,
    /// 程序服务（schema 后置；预留）。
    Service,
}

impl EndpointClass {
    /// 按内容类型推导能力族（`MediaKind` 是 wire 真源，本枚举是其分组视图）。
    pub const fn from_kind(kind: MediaKind) -> Self {
        match kind {
            MediaKind::Screen | MediaKind::Window | MediaKind::Camera => Self::Graph,
            MediaKind::Mic | MediaKind::SystemAudio => Self::Audio,
            MediaKind::File => Self::File,
            MediaKind::Clipboard => Self::Clipboard,
            MediaKind::Input => Self::Input,
            MediaKind::Service => Self::Service,
        }
    }
}

/// load 探测函数：平台适应层注入（查环境 / 设备 / 权限），内核零 OS 调用。
pub type Probe = Arc<dyn Fn() -> StdResult<(), String> + Send + Sync>;

/// 端点公共身份 + 挂载状态（各具体端点的共有字段）。
#[derive(Debug)]
pub struct EndpointBase {
    pub id: EndpointId,
    pub kind: MediaKind,
    pub name: String,
    /// 能否被挂载成节点（load 探测结果；false = 不可通告、不可订阅）。
    pub available: bool,
    /// load/share 失败原因（UI / 目录展示）。
    pub last_error: Option<String>,
}

impl EndpointBase {
    /// 标记不可挂载并记录原因（load / share 失败时）。
    pub fn mark_failed(&mut self, e: String) {
        self.available = false;
        self.last_error = Some(e);
    }
}

/// 端点公共身份 + 能力族契约（分享端 / 订阅端共用的**视图契约**）。
///
/// 端点分两个真契约（docs/endpoint-model-v2.md §3 演进——分享端与订阅端
/// **本质不同**：分享端是内容源（摄像机/屏幕/麦克风），订阅端是内容宿
/// （播放器/文件接收））：
///
/// * [`ShareEndpoint`]：可被订阅——`load` 探测挂载性 + `share` 开推；
/// * [`SubscribeEndpoint`]：主动订阅——`subscribe` 消费流并还原；
///
/// 本 trait 只承载两端的共同视图（身份 / 内容类型 / 能力族 / 策略档案），
/// 注册表与 UI 按它展示，不再有「双向能力体」的无意义占位方法。
pub trait Endpoint: Send + Sync {
    /// 节点内稳定身份（`EndpointId`：`kind` + 数值子 id；跨设备唯一性由
    /// `(device_id, endpoint_id)` 命名空间保证）。订阅端复用目标端点的
    /// `EndpointId`（仅日志用途，不进注册表）。
    fn id(&self) -> EndpointId;
    fn kind(&self) -> MediaKind;
    /// 用户可见名。
    fn name(&self) -> &str;
    /// 能力族（按数据形态分族；同一族分享/订阅共享一类实现，
    /// 默认按 kind 推导）。
    fn class(&self) -> EndpointClass {
        EndpointClass::from_kind(self.kind())
    }
    /// 目标类型（确定目标 / 实时目标）：决定共享生命周期（是否 watchers
    /// 自动收尾）。策略（传输/pick）已由端点自主声明，不按 TargetKind 推导。
    fn target(&self) -> TargetKind;
    /// 传输层可靠性档案（通信模式 v2，docs/comm-mode-v2.md §3）：**端点自主
    /// 指定策略标记**——协商时随清单上报，内核按它装载传输模块，不猜测。
    /// 实时媒体 Lossy（允许丢包）；文件/剪贴板 Lossless（不允许丢包）；
    /// 弱网流可声明 Adaptive。**不进注册表**（端点声明、传输模块执行）。
    fn transport_profile(&self) -> ReliabilityProfile;
    /// 端点自主声明的策略组合（序列化规则 + pick 规则；v2 组合方法，替代 v1
    /// 的 `pick_rule()` 散方法）。注册表按策略 id 记录，订阅按
    /// `(节点, 端点, 策略)` 精确取（docs/endpoint-model-v2.md §2）。
    fn strategy(&self) -> EndpointStrategy;
}

/// 分享端点（内容源）：可被订阅——自维护「可挂载性」+ 订阅达成后开推。
///
/// 每个分享端点实现约定行为（与内核的约定，非语言特性）：
/// * `load`：探测自身可用性（能否被挂载成节点），维护 `available` / `last_error`；
/// * `share`：订阅达成后启动共享（推流），类型自决，内核不做分派。
///
/// `available() == false` 时不可通告、不可订阅。
pub trait ShareEndpoint: Endpoint {
    /// 能否被挂载成节点（load 探测结果）。
    fn available(&self) -> bool;
    /// load/share 失败原因。
    fn last_error(&self) -> Option<&str>;
    /// load：探测自身可用性；失败 → `available=false` + 记录 `last_error`。
    fn load(&mut self) -> StdResult<(), String>;
    /// 订阅达成后启动共享（内部自行 spawn 异步推流）。
    fn share(&self, app: Arc<dyn EndpointApp>, ctx: SubscribeCtx);
}

/// 订阅端点（内容宿）：主动订阅别人并处理（端点作为宿主处理订阅流/数据，
/// 如播放器渲染 / 文件接收落盘）。
///
/// 由内核按注册表 `(节点, 端点, 策略)` 解析后**生成**（订阅端点生成），
/// 与分享端完全解耦——订阅端不是"某个分享端点的另一半"，而是独立的类
/// （Graph/Audio → 播放器，File → 文件接收）。
pub trait SubscribeEndpoint: Endpoint {
    /// 主动订阅（[`SubscribeSpec`] 携带解析好的策略组合与数据面入口），
    /// 内部自行 spawn 异步处理。
    fn subscribe(&self, app: Arc<dyn EndpointApp>, spec: SubscribeSpec);
}

/// 媒体源分享端点（能力族：Graph / Audio 类的**分享端**统一实现）。
///
/// 端点只声明「视频源 + 可选伴音」，`share` / 策略 / 传输 / 目标类型由本类
/// 提供默认实现（组 [`StreamConfig`] 调内核推流）——屏幕（Graph，纯视频）、
/// 麦克风 / 系统声音（Audio，纯音频）不再各写样板：
///
/// ```ignore
/// impl MediaSourceEndpoint for ScreenEndpoint {
///     fn video(&self) -> Option<VideoSource> { Some(VideoSource::Screen) }
///     fn audio(&self) -> Option<AudioSourceConfig> { None }
/// }
/// ```
///
/// 订阅端对应类实现见 `stross_endpoint::subscribe`（Graph/Audio 统一接收播放）。
pub trait MediaSourceEndpoint: ShareEndpoint {
    /// 视频源（`None` = 纯音频源）。
    fn video(&self) -> Option<VideoSource>;
    /// 伴音（`None` = 无音频轨）。
    fn audio(&self) -> Option<AudioSourceConfig>;

    /// 媒体源一律实时目标（持续推流，watchers 自动收尾）。
    fn target(&self) -> TargetKind {
        TargetKind::Live
    }
    /// 媒体源默认允许丢包（实时音视频：丢帧/丢块靠关键帧对齐自愈）。
    fn transport_profile(&self) -> ReliabilityProfile {
        ReliabilityProfile::Lossy
    }
    /// 媒体源默认策略：直通序列化 + 严格即时（Realtime）。
    fn strategy(&self) -> EndpointStrategy {
        EndpointStrategy::passthrough(PickRule::Realtime)
    }
    /// 分享端统一实现：组流推本机中继（订阅驱动只走 pull）。
    fn share(&self, app: Arc<dyn EndpointApp>, ctx: SubscribeCtx) {
        spawn_media_share(
            app,
            ctx,
            self.id(),
            self.name().to_string(),
            self.video(),
            self.audio(),
        );
    }
}

/// 媒体源分享端点的 [`Endpoint`] + [`ShareEndpoint`] 完整样板：共享方法
/// （target / transport_profile / strategy / share）统一委托到
/// [`MediaSourceEndpoint`] 默认实现——逻辑单一真源；端点只传「身份」
/// （id/kind/name）与「挂载性」（available/last_error/load）两组方法体。
///
/// ```ignore
/// impl_media_source_endpoint!(ScreenEndpoint {
///     fn id(&self) -> EndpointId { self.base.id }
///     fn kind(&self) -> MediaKind { self.base.kind }
///     fn name(&self) -> &str { &self.base.name }
/// }, {
///     fn available(&self) -> bool { self.base.available }
///     fn last_error(&self) -> Option<&str> { self.base.last_error.as_deref() }
///     fn load(&mut self) -> StdResult<(), String> { /* 探测 */ }
/// });
/// ```
#[macro_export]
macro_rules! impl_media_source_endpoint {
    ($t:ty { $( $endpoint_body:item )* }, { $( $share_body:item )* }) => {
        impl $crate::contract::Endpoint for $t {
            fn target(&self) -> $crate::contract::TargetKind {
                $crate::contract::MediaSourceEndpoint::target(self)
            }
            fn transport_profile(&self) -> stross_proto::message::ReliabilityProfile {
                $crate::contract::MediaSourceEndpoint::transport_profile(self)
            }
            fn strategy(&self) -> stross_proto::message::EndpointStrategy {
                $crate::contract::MediaSourceEndpoint::strategy(self)
            }
            $( $endpoint_body )*
        }
        impl $crate::contract::ShareEndpoint for $t {
            fn share(
                &self,
                app: std::sync::Arc<dyn $crate::contract::EndpointApp>,
                ctx: $crate::contract::SubscribeCtx,
            ) {
                $crate::contract::MediaSourceEndpoint::share(self, app, ctx)
            }
            $( $share_body )*
        }
    };
}

/// 订阅事件载荷：端点 `share` 开推的依据（协商层授予成功后构造，
/// docs/endpoint-model-v2.md §4 联动）。
#[derive(Debug, Clone)]
pub struct SubscribeCtx {
    /// 订阅方节点 device_id。
    pub subscriber: String,
    /// 公开方定稿后的数据面方向。
    pub delivery: Delivery,
    /// 数据面流 id：pull = 公开方本机会话（内核预授权）；push = 订阅方自签会话。
    pub stream_id: String,
    /// 协商定稿的传输层可靠性档案（允许丢包/不允许丢包/自适应）；
    /// 端点据此装载对应传输模块（通信模式 v2，docs/comm-mode-v2.md §3）。
    pub transport_profile: ReliabilityProfile,
    /// 协商定稿的策略组合（序列化规则 + pick 规则；注册表
    /// `(节点, 端点, 策略)` 解析结果）；发送侧装载逻辑与接收侧解读模块共用。
    pub strategy: EndpointStrategy,
    /// push 模式：订阅方中继 HTTP 基址（`ws://ip:port`；公开方出站 push 目标）。
    pub relay_addr: Option<String>,
    /// push 模式：订阅方自签的一次性接入凭证（推流 Hello 出示）。
    pub share_token: Option<String>,
}

/// 端点可见的内核调度能力（**内核实现**；端点层只依赖此契约，不依赖内核类型）。
///
/// 内核 = 纯管理调度：会话 / 鉴权 / 路由 / 注册表。数据面的执行细节
/// （ffmpeg 子进程、portal+pipewire 采集、文件泵）都在端点层。
#[async_trait]
pub trait EndpointApp: Send + Sync {
    /// 启动一次媒体推流（pull = 推本机中继，`relay_url=None`；push = 出站）。
    async fn start_stream(
        &self,
        cfg: StreamConfig,
        relay_url: Option<String>,
    ) -> anyhow::Result<crate::StartResult>;
    /// 内嵌中继端口（未锚定/未启动时为 `None`）。
    fn relay_port(&self) -> Option<u16>;
    /// 文件泵推送（文件端点确定目标的一次推送；返回发送字节数）。
    async fn push_file(&self, path: PathBuf, opts: FilePushOptions) -> anyhow::Result<u64>;
    /// 文件接收（订阅端文件端点确定目标的一次接收；返回落盘结果）。
    /// 端点 `subscribe`（订阅端）经此把订阅流落盘——与 `push_file` 同构：
    /// 内核提供调度能力，端点自驱动。
    async fn receive_file(
        &self,
        watch_url: String,
        stream_id: String,
        out_dir: PathBuf,
    ) -> anyhow::Result<crate::ReceivedFile>;
    /// 媒体接收（订阅端 Graph / Audio 类的执行，播放器入端点）：连公开方
    /// 中继接收流、按订阅规格的 pick 规则解读并解码，**阻塞到流结束**，
    /// 返回解码帧数（0 = 流无视频帧/纯音频）。
    ///
    /// 与 `push_file`/`receive_file` 同构：内核提供调度能力（桌面走
    /// `Receiver` + 解码），订阅端点自驱动。壳层 GUI 播放路径
    /// （`start_receive` 命令 + 画布）保留不动。
    async fn receive_media(&self, spec: &SubscribeSpec) -> anyhow::Result<u64>;
    /// 端点自驱动辅助：在运行时上下文执行一个异步任务（`share`/`subscribe`
    /// 的 fire-and-forget 载体）。内核实现为 `tokio::spawn`（壳层注入运行时）。
    fn spawn_task(&self, fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>);
    /// 端点共享登记：媒体端点 `start_stream` 成功后回调（实时目标生命周期治理用——
    /// watchers=0 自动收尾 / 取消通告联动停止 / 同端点订阅收敛）。
    ///
    /// `self_weak`：发起共享的内核弱引用（生命周期治理任务在无观看者时经它回调
    /// [`Self::stop_share_if_unwatched`]，避免任务持有强引用拖住内核）。
    /// 默认空实现：不登记的端点（文件等有完成态）不受 watchers 自动停止影响。
    fn note_share_active(
        &self,
        _self_weak: std::sync::Weak<dyn EndpointApp>,
        _endpoint_id: EndpointId,
        _stream_id: &str,
        _delivery: Delivery,
    ) {
    }
    /// 端点共享停止回调：watchers 归零复查确认无人观看后调用（默认空实现）。
    fn stop_share_if_unwatched(&self, _stream_id: &str) {}
}

/// 媒体端点自动推流（实时目标共用）：pull 推本机中继（地址自动），
/// push 凭订阅方凭证出站推入订阅方中继。
///
/// `endpoint_id`：端点身份（共享登记用；见 [`EndpointApp::note_share_active`]）。
/// 经 [`EndpointApp::spawn_task`] 在运行时上下文执行（契约层零 tokio 依赖，
/// 运行时由内核注入）。
pub fn spawn_media_share(
    app: Arc<dyn EndpointApp>,
    ctx: SubscribeCtx,
    endpoint_id: EndpointId,
    title: String,
    video: Option<VideoSource>,
    audio: Option<AudioSourceConfig>,
) {
    let self_weak = std::sync::Arc::downgrade(&app);
    let app2 = app.clone();
    app.spawn_task(Box::pin(async move {
        let cfg = StreamConfig {
            stream_id: ctx.stream_id.clone(),
            title,
            video,
            quality: Quality::MEDIUM,
            audio,
            duration_secs: None,
            // 订阅驱动定稿（docs/endpoint-model-v2.md §4）：只走 pull——推本机
            // 中继，无出站凭证。
            share_token: None,
        };
        let relay_url = resolve_media_url(&ctx);
        match app2.start_stream(cfg, relay_url).await {
            Ok(r) => {
                tracing::info!(
                    "端点已自动推流: stream={} 订阅方 {}",
                    r.stream_id,
                    ctx.subscriber
                );
                // 生命周期治理登记（watchers=0 自动收尾 / 取消通告联动停止 / 订阅收敛）
                app2.note_share_active(self_weak, endpoint_id, &r.stream_id, ctx.delivery);
            }
            Err(e) => tracing::warn!("端点自动推流失败（订阅方 {}）: {e:#}", ctx.subscriber),
        }
    }));
}

/// 媒体推流的目标地址：订阅驱动定稿只走 pull → `None`（推本机中继，地址由
/// 内核自动选择；无 push 出站路径）。
pub fn resolve_media_url(_ctx: &SubscribeCtx) -> Option<String> {
    None
}

/// 文件泵推送地址：订阅驱动定稿只走 pull → 自己的受控中继（回环地址）。
pub fn resolve_file_url(app: &dyn EndpointApp, _ctx: &SubscribeCtx) -> Option<String> {
    let port = app.relay_port()?;
    Some(format!("ws://127.0.0.1:{port}/ws/push"))
}

/// 观看数轮询基址（文件泵等观看者接入用）：订阅驱动定稿只走 pull → 自己中继。
pub fn resolve_watcher_base(app: &dyn EndpointApp, _ctx: &SubscribeCtx) -> Option<String> {
    app.relay_port().map(|p| format!("ws://127.0.0.1:{p}"))
}

// ---------------------------------------------------------------------------
// 数据契约：分享端与订阅端之间传输的纯数据载荷（原 stross-endpoint pipeline）
// ---------------------------------------------------------------------------

/// 画质档位。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Quality {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

impl Quality {
    pub const LOW: Self = Self {
        width: 640,
        height: 360,
        fps: 24,
        bitrate_kbps: 800,
    };
    pub const MEDIUM: Self = Self {
        width: 1280,
        height: 720,
        fps: 30,
        bitrate_kbps: 2500,
    };
    pub const HIGH: Self = Self {
        width: 1920,
        height: 1080,
        fps: 30,
        bitrate_kbps: 6000,
    };

    /// 预设列表 `(显示名, 配置)`。
    pub const fn presets() -> [(&'static str, Self); 3] {
        [
            ("低 (640×360@24)", Self::LOW),
            ("中 (1280×720@30)", Self::MEDIUM),
            ("高 (1920×1080@30)", Self::HIGH),
        ]
    }

    /// GOP（关键帧间隔，帧数），默认 2 秒。
    pub fn gop(&self) -> u32 {
        (self.fps * 2).max(1)
    }
}

impl Default for Quality {
    fn default() -> Self {
        Self::HIGH
    }
}

/// 视频源。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum VideoSource {
    /// 整个主屏幕（Windows: gdigrab；Linux: x11grab）。
    Screen,
    /// 摄像头；`device` 为 `CameraDevice.id`。
    Camera { device: Option<String> },
    /// lavfi 测试画面（如 `testsrc2`、`smptebars`），方便无设备时演示。
    Synthetic { pattern: String },
}

/// 音频源配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AudioSourceConfig {
    /// 麦克风设备；`None` = 系统默认输入。
    pub mic: Option<String>,
    /// 系统声音（回环采集设备）；`None` = 不采集。
    pub system_audio: Option<String>,
    /// 合成音源（lavfi `sine`，频率 Hz）；`Some` 时取代真实采集，
    /// 无设备环境测试 / 演示用（见播放侧解码回路的集成测试）。
    #[serde(default)]
    pub synthetic: Option<u32>,
    #[serde(default = "default_sample_rate")]
    pub sample_rate: u32,
    #[serde(default = "default_channels")]
    pub channels: u8,
    #[serde(default = "default_audio_bitrate")]
    pub bitrate_kbps: u32,
}

const fn default_sample_rate() -> u32 {
    48_000
}
const fn default_channels() -> u8 {
    2
}
const fn default_audio_bitrate() -> u32 {
    128
}

impl Default for AudioSourceConfig {
    fn default() -> Self {
        Self {
            mic: None,
            system_audio: None,
            synthetic: None,
            sample_rate: default_sample_rate(),
            channels: default_channels(),
            bitrate_kbps: default_audio_bitrate(),
        }
    }
}

impl AudioSourceConfig {
    /// 合成测试音（440Hz sine）：无设备环境下验证音频链路。
    ///
    /// `--audio` 类 CLI 参数用它——此前直接用 [`AudioSourceConfig::default`]
    /// 导致 synthetic/mic/system_audio 全为 `None`，ffmpeg 无音频输入，
    /// 推流实际无声（音频链路从未被真实数据验证，D3 反向音频验收的前提）。
    pub fn synthetic_test() -> Self {
        Self {
            synthetic: Some(440),
            ..Self::default()
        }
    }
}

/// 一次推流的完整配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamConfig {
    pub stream_id: String,
    pub title: String,
    #[serde(default)]
    pub video: Option<VideoSource>,
    #[serde(default)]
    pub quality: Quality,
    #[serde(default)]
    pub audio: Option<AudioSourceConfig>,
    /// 限制推流时长（秒）；`None` = 无限。测试/演示用。
    #[serde(default)]
    pub duration_secs: Option<u32>,
    /// 一次性接入凭证（跨设备推流到对方受控中继用；本机推流为 `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share_token: Option<String>,
}

impl StreamConfig {
    /// CLI 合成源推流配置（测试 / 演示：testsrc2 画面 + 可选 440Hz 测试音）。
    ///
    /// `push` / `ctrl start-stream` / `demo_push` 共用，避免各处手拼字段
    /// （重复实现，曾出现 `--audio` 无声等不一致）。
    pub fn cli_synthetic(
        stream_id: String,
        title: String,
        quality: Quality,
        secs: u32,
        audio: bool,
        share_token: Option<String>,
    ) -> Self {
        let mut cfg = Self {
            stream_id,
            title,
            video: Some(VideoSource::Synthetic {
                pattern: "testsrc2".into(),
            }),
            quality,
            audio: None,
            duration_secs: Some(secs),
            share_token,
        };
        if audio {
            cfg.audio = Some(AudioSourceConfig::synthetic_test());
        }
        cfg
    }

    /// 生成推流端注册用的 `Hello` 控制消息。
    pub fn hello(&self) -> ControlMessage {
        ControlMessage::Hello {
            stream_id: self.stream_id.clone(),
            title: self.title.clone(),
            video: self.video_track_info(),
            audio: self.audio_track_info(),
            share_token: self.share_token.clone(),
        }
    }

    /// 生成 Hello 消息里的轨道信息（供观看端展示）。
    pub fn video_track_info(&self) -> Option<TrackInfo> {
        self.video.as_ref().map(|_| TrackInfo {
            codec: CodecId::H264,
            width: Some(self.quality.width),
            height: Some(self.quality.height),
            fps: Some(self.quality.fps),
            sample_rate: None,
            channels: None,
        })
    }

    pub fn audio_track_info(&self) -> Option<TrackInfo> {
        self.audio.as_ref().map(|a| TrackInfo {
            codec: CodecId::Aac,
            width: None,
            height: None,
            fps: None,
            sample_rate: Some(a.sample_rate),
            channels: Some(a.channels),
        })
    }
}

/// 文件泵参数（公开方驱动构造；内核 `push_file` 消费——契约单一真源）。
#[derive(Debug, Clone)]
pub struct FilePushOptions {
    /// 中继推流地址（`ws://host:port/ws/push`；文件走无损 WS 路径）。
    pub push_url: String,
    /// 数据面流 id（pull = 公开方本机会话；push = 订阅方自签会话）。
    pub stream_id: String,
    /// 推流标题（Hello.title；展示用）。
    pub title: String,
    /// 跨设备接入凭证（push 模式 = 订阅方自签；本机 pull = `None`）。
    pub share_token: Option<String>,
    /// 观看数轮询基址（`ws://host:port`；`None` = 不等观看者直接推）。
    pub watcher_base: Option<String>,
}
