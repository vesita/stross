//! 端点 SPI（分享端 / 订阅端契约）——**内核约定特性、端点实现、内核只基于
//! 特性行动**（docs/framework-v3.md §3.2）。
//!
//! v3：契约真源随概念 crate 同仓（`stross-endpoint` 契约 + 实现同仓），
//! stross-types 仅为过渡重导出层（P2 删除）。内核（stross-kernel）声明它
//! 需要什么（[`ShareEndpoint`] / [`SubscribeEndpoint`] / 四个能力 trait），
//! 端点实现这些特性；两端都只依赖本契约与 wire 层（stross-proto）+ 展示
//! 视图（stross-view），互不依赖对方的具体类型。
//!
//! * [`Endpoint`]：公共视图（身份 / 内容类型 / 能力族 / 策略档案）
//! * [`ShareEndpoint`]（分享端点 = 内容源）：`load` 探测 + `share` 开推
//! * [`SubscribeEndpoint`]（订阅端点 = 内容宿）：`subscribe` 消费流并还原
//! * [`MediaSourceEndpoint`]：Graph / Audio 类分享端的族实现（只声明
//!   `video()`/`audio()`，share/策略/传输族默认）
//! * 四个能力 trait（[`StreamHost`] / [`FileHost`] / [`MediaHost`] / [`Runtime`]，
//!   组合 [`ShareHost`] / [`SubscribeHost`]）：端点可见的内核调度能力（内核
//!   实现）——端点只拿自己需要的能力，不见内核整张脸（v3 §3.2 取代旧聚合
//!   `EndpointApp`）
//! * 数据契约（[`crate::data`]）：[`StreamConfig`] / [`VideoSource`] /
//!   [`AudioSourceConfig`] / [`Quality`] / [`FilePushOptions`] —— 分享端与
//!   订阅端之间传输的纯数据载荷（serialize + pick 的策略组合在 stross-proto）

use std::path::PathBuf;
use std::result::Result as StdResult;
use std::sync::Arc;

use async_trait::async_trait;

use stross_proto::message::{
    Delivery, EndpointId, EndpointStrategy, MediaKind, NodeId, PickRule, ReliabilityProfile,
    StreamId, SubscribeSpec,
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

/// 端点能力类别（按数据形态分族，docs/framework-v3.md §3.2）。
///
/// **同一族 = 同一类分享/订阅实现**：分享端（[`MediaSourceEndpoint`] 统一
/// 「组流推流」）与订阅端（订阅端点生成按族分发）都按族共享，消灭逐个端点
/// 的样板代码。**不进 wire**（由 [`MediaKind`] 推导，避免冗余字段）。
///
/// `Hash`：用作注册表键（v3 §2.2 策略注册表模式——订阅端点生成工厂表
/// `HashMap<EndpointClass, SubscribeEndpointFactory>` 的**强类型枚举键**）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
/// 端点分两个真契约（v2.1 演进——分享端与订阅端**本质不同**：分享端是
/// 内容源（摄像机/屏幕/麦克风），订阅端是内容宿（播放器/文件接收））：
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
    /// 传输层可靠性档案（v3 概念：端点自主指定策略标记——协商时随清单上报，
    /// 内核按它装载传输模块，不猜测）。实时媒体 Lossy；文件/剪贴板 Lossless；
    /// 弱网流可声明 Adaptive。**不进注册表**（端点声明、传输模块执行）。
    fn transport_profile(&self) -> ReliabilityProfile;
    /// 端点自主声明的策略组合（序列化规则 + pick 规则）。注册表按策略 id
    /// 记录，订阅按 `(节点, 端点, 策略)` 精确取。
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
    ///
    /// `host`：分享端可见的内核能力组合 [`ShareHost`]（= [`StreamHost`] +
    /// [`FileHost`]）——媒体端点（屏幕/音频）只用 `StreamHost` 部分
    /// （`start_stream` / `relay_port`），文件端点（[`crate::share::FileEndpoint`]）
    /// 还要 `FileHost` 部分（`push_file`）；`runtime`：fire-and-forget 载体。
    fn share(&self, host: Arc<dyn ShareHost>, runtime: Arc<dyn Runtime>, ctx: SubscribeCtx);
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
    ///
    /// `host`：订阅端可见的内核能力组合 [`SubscribeHost`]（= [`MediaHost`] +
    /// [`FileHost`]）——媒体订阅端（播放器）用 `MediaHost` 部分
    /// （`receive_media`），文件订阅端用 `FileHost` 部分（`receive_file`）；
    /// `runtime`：fire-and-forget 载体。
    fn subscribe(
        &self,
        host: Arc<dyn SubscribeHost>,
        runtime: Arc<dyn Runtime>,
        spec: SubscribeSpec,
    );
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
    /// 媒体端点只用 `host` 的 [`StreamHost`] 部分（`start_stream`）——
    /// 组合 [`ShareHost`] 在此 upcast 为 `Arc<dyn StreamHost>` 交给辅助函数。
    fn share(&self, host: Arc<dyn ShareHost>, runtime: Arc<dyn Runtime>, ctx: SubscribeCtx) {
        // trait upcast（ShareHost: StreamHost，Rust 1.86+）：媒体端点只要推流能力
        let host: Arc<dyn StreamHost> = host;
        crate::data::spawn_media_share(
            &host,
            &runtime,
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
                host: std::sync::Arc<dyn $crate::contract::ShareHost>,
                runtime: std::sync::Arc<dyn $crate::contract::Runtime>,
                ctx: $crate::contract::SubscribeCtx,
            ) {
                $crate::contract::MediaSourceEndpoint::share(self, host, runtime, ctx)
            }
            $( $share_body )*
        }
    };
}

/// 订阅事件载荷：端点 `share` 开推的依据（协商层授予成功后构造，
/// docs/framework-v3.md §3.2 联动）。
#[derive(Debug, Clone)]
pub struct SubscribeCtx {
    /// 订阅方节点 device_id。
    pub subscriber: NodeId,
    /// 公开方定稿后的数据面方向。
    pub delivery: Delivery,
    /// 数据面流 id：pull = 公开方本机会话（内核预授权）；push = 订阅方自签会话。
    pub stream_id: StreamId,
    /// 端点据此装载对应传输模块。
    pub transport_profile: ReliabilityProfile,
    /// 协商定稿的策略组合（序列化规则 + pick 规则）；发送侧装载逻辑与接收侧
    /// 解读模块共用。
    pub strategy: EndpointStrategy,
    /// push 模式：订阅方中继 HTTP 基址（`ws://ip:port`；公开方出站 push 目标）。
    pub relay_addr: Option<String>,
    /// push 模式：订阅方自签的一次性接入凭证（推流 Hello 出示）。
    pub share_token: Option<String>,
}

// ---------------------------------------------------------------------------
// 内核调度能力 = 四个小 trait（v3 §3.2 取代旧聚合 `EndpointApp`）——
// 端点只拿自己需要的能力，不见内核整张脸；每个 trait 对象安全、面最小。
// 生命周期治理（watchers=0 自动收尾 / 取消通告联动停止）**从契约删除**
// （归未来 `stross-share::ShareService`，docs/framework-v3.md §3.3）。
// ---------------------------------------------------------------------------

/// 媒体推流能力（分享端媒体端点可见的内核能力）。
#[async_trait]
pub trait StreamHost: Send + Sync {
    /// 启动一次媒体推流（pull = 推本机中继，`relay_url=None`；push = 出站）。
    async fn start_stream(
        &self,
        cfg: StreamConfig,
        relay_url: Option<String>,
    ) -> anyhow::Result<stross_view::StartResult>;
    /// 内嵌中继端口（未锚定/未启动时为 `None`）。
    fn relay_port(&self) -> Option<u16>;
}

/// 文件传输能力（文件端点可见的内核能力）。
#[async_trait]
pub trait FileHost: Send + Sync {
    /// 文件泵推送（文件端点确定目标的一次推送；返回发送字节数）。
    async fn push_file(&self, path: PathBuf, opts: FilePushOptions) -> anyhow::Result<u64>;
    /// 文件接收（订阅端文件端点确定目标的一次接收；返回落盘结果）。
    /// 端点 `subscribe`（订阅端）经此把订阅流落盘——与 `push_file` 同构：
    /// 内核提供调度能力，端点自驱动。
    async fn receive_file(
        &self,
        watch_url: String,
        stream_id: StreamId,
        out_dir: PathBuf,
    ) -> anyhow::Result<stross_view::ReceivedFile>;
}

/// 媒体接收能力（订阅端 Graph / Audio 类可见的内核能力）。
#[async_trait]
pub trait MediaHost: Send + Sync {
    /// 媒体接收（订阅端 Graph / Audio 类的执行，播放器入端点）：连公开方
    /// 中继接收流、按订阅规格的 pick 规则解读并解码，**阻塞到流结束**，
    /// 返回解码帧数（0 = 流无视频帧/纯音频）。
    ///
    /// 与 `push_file`/`receive_file` 同构：内核提供调度能力（桌面走
    /// `Receiver` + 解码），订阅端点自驱动。壳层 GUI 播放路径
    /// （`start_receive` 命令 + 画布）保留不动。
    async fn receive_media(&self, spec: &SubscribeSpec) -> anyhow::Result<u64>;
}

/// 运行时载体（端点自驱动）：在运行时上下文执行一个异步任务（`share`/
/// `subscribe` 的 fire-and-forget 载体）。内核实现为 `tokio::spawn`。
pub trait Runtime: Send + Sync {
    fn spawn_task(&self, fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>);
}

/// 分享端可见的内核能力组合（[`ShareEndpoint::share`] 的 `host` 参数）：
/// 推流（[`StreamHost`]）+ 文件泵（[`FileHost`]）。
///
/// 媒体端点（屏幕/音频）只用 [`StreamHost`] 部分（`start_stream` /
/// `relay_port`）；文件端点（[`crate::share::FileEndpoint`]）还要 [`FileHost`]
/// 部分（`push_file`）——`share` 签名以本组合 trait 给出，实现方（内核）
/// 同时实现 [`StreamHost`] + [`FileHost`] 即自动满足（blanket impl），
/// 两端点各取所需，不再聚合出「内核整张脸」。
///
/// 这是对 v3 §3.2 字面（`share(&self, host: Arc<dyn StreamHost>, …)`）的唯一
/// 微调：文件端点 `share` 需要 `push_file`（[`FileHost`]），媒体端点需要
/// `start_stream`（[`StreamHost`]），二者在统一签名里以组合 trait 收敛。
pub trait ShareHost: StreamHost + FileHost {}

impl<T: StreamHost + FileHost> ShareHost for T {}

/// 订阅端可见的内核能力组合（[`SubscribeEndpoint::subscribe`] 的 `host` 参数）：
/// 媒体接收（[`MediaHost`]）+ 文件接收（[`FileHost`]）。
///
/// 媒体订阅端（[`crate::subscribe::MediaReceiveEndpoint`]）用 [`MediaHost`] 部分
/// （`receive_media`）；文件订阅端（[`crate::subscribe::FileReceiveEndpoint`]）
/// 用 [`FileHost`] 部分（`receive_file`）——`subscribe` 签名以本组合 trait
/// 给出，实现方（内核）同时实现 [`MediaHost`] + [`FileHost`] 即自动满足
/// （blanket impl），与分享端 [`ShareHost`] 同一模式。
pub trait SubscribeHost: MediaHost + FileHost {}

impl<T: MediaHost + FileHost> SubscribeHost for T {}

// 数据契约（Quality/VideoSource/StreamConfig 等）与自动推流辅助在 [`crate::data`]；
// 本模块重导出保持 `stross_endpoint::contract::StreamConfig` 等路径可用。
pub use crate::data::{
    AudioSourceConfig, FilePushOptions, Quality, StreamConfig, VideoSource, resolve_file_url,
    resolve_media_url, resolve_watcher_base, spawn_media_share,
};
