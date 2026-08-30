//! # stross-kernel —— Stross 内核
//!
//! **内核 = 全部平台无关的服务提供**。数据面、信令面、端点框架、发现、
//! 推流/接收编排都在这里，以 [`Kernel`] 门面为单一入口：
//!
//! * [`relay`]：中继服务器 + 中继 HTTP 客户端（数据面，服务端契约单一真源）
//! * [`pick`]：pick 规则层（装载/解读语义：严格即时 / 严格顺序，docs/comm-mode-v2.md §3.0）
//! * [`sender`] / [`watch`]：推流客户端与观看链路
//! * [`discovery`]：mDNS 服务发现（浏览 + 广播）
//! * [`negotiator`] / [`negotiator_client`]：凭证自动协商（服务端 + 客户端）
//! * [`control`]：控制面（服务端 + 客户端，仅回环）
//! * [`subscriber`] / [`file_xfer`]：订阅方编排与文件端点传输
//! * [`devices`]：局域网设备扫描聚合（mDNS + 探测，一站式）
//! * [`bootstrap`]：引导层（锚定 + 目录 + 握手端点的启动原语）
//! * [`engine`] / [`receiver`]：推流引擎与接收编排（`stross-media` 能力之上）
//! * [`kernel`]：内核门面 [`Kernel`]（会话 / 路由 / 鉴权 / 凭证 / 端点 /
//!   推流 / 接收 / 身份），事件经 [`KernelEvent`] 广播给 UI
//!
//! ## 分层
//!
//! ```text
//! stross-proto（线协议类型）
//!   └─ stross-transport（传输抽象 + SRT/QUIC/WS/WebRTC、RelayUrl、net）
//!        └─ stross-kernel（本 crate：所有服务，零路径 / 零 OS 调用 / 零平台分支）
//!             ├─ stross-media（采集/播放能力 trait 与后端）
//!             └─ stross-bridge（平台适应：paths / hostname / 平台设备枚举）
//!                  └─ 壳层（CLI / GUI / 独立中继：参数解析 + 展示 + 平台适配）
//! ```
//!
//! 内核严格零路径约定 / 零 `hostname::get()` 类 OS 调用 / 零 `cfg(target_os)`
//! 逻辑分支（仅播放能力可用性一处 `cfg(not(android))`，属能力交付）。
//! 平台知识一律经 [`stross_bridge`] 注入（base_dir / hostname / 设备清单）。

pub mod bootstrap;
pub mod control;
#[cfg(feature = "discovery")]
pub mod discovery;
pub mod engine;
pub mod error;
pub mod file_xfer;
pub mod kernel;
mod lock;
pub mod negotiator;
pub mod negotiator_client;
pub mod pick;
pub mod receiver;
pub mod relay;
pub mod sender;
pub mod settings;
pub mod subscriber;
pub mod view;
pub mod watch;

// 传输层（独立 crate stross-transport）：
// `stross_kernel::transport::*` 与 `stross_kernel::net::*` 路径保持兼容。
pub use stross_transport as transport;
pub use stross_transport::net;
pub use stross_transport::{DataSession, SessionPacket, Transport, TransportError, TransportStats};

pub use control::{CtrlRequest, CtrlResponse, CtrlServer, DEFAULT_CTRL_PORT};
// 发现子系统（discovery v0.2.0）：扫描聚合 + 统一发现清单（需 discovery feature）。
#[cfg(feature = "discovery")]
pub use discovery::{ScannedDevice, StreamView, probe_base, scan, scan_lan, to_views};
pub use engine::SenderEngine;
pub use error::{Error, RelayOpError, Result, WatchError};
pub use file_xfer::{ReceivedFile, receive_file, receive_file_session};
pub use kernel::{
    AuthError, AuthPolicy, DataPlaneBackend, EndpointEntry, EndpointRegistration, EndpointRegistry,
    FileSource, Kernel, KernelEvent, NodeInfo, NodeRegistration, NodeRole, PinAuthPolicy,
    RelayDataPlane, Session, SessionPrefs, TransportAddr, UnifiedRegistry,
};
pub use negotiator::{
    CliUi, DEFAULT_NEGOTIATOR_PORT, DeviceIdentity, NegotiatorUi, NoopUi, PendingRequest,
    RelayAddr, ShareGrant, ShareNegotiator, ShareRequest, TrustStore, load_or_create_identity,
};
pub use negotiator_client::request_grant;
pub use receiver::{ReceiveStats, Receiver};
pub use relay::{DEFAULT_PORT, GUI_PORT, RelayHandle, RelayServer};
pub use sender::RelayClient;
pub use settings::{Settings, load_or_default as load_settings, save as save_settings};
pub use stross_endpoint::contract::{
    resolve_file_url, resolve_media_url, resolve_watcher_base, spawn_media_share,
};
pub use stross_endpoint::share::file::FilePushOptions;
/// 端点层（插件区）契约与端点实现重导出：保持 `stross_kernel::Xxx` 路径兼容
/// （定义单一真源在 stross-endpoint crate；内核 = 管理调度，消费其契约）。
pub use stross_endpoint::{
    Endpoint, EndpointApp, EndpointBase, EndpointClass, FileEndpoint, FileReceiveEndpoint,
    MediaReceiveEndpoint, MediaSourceEndpoint, MicEndpoint, Probe, ScreenEndpoint, ShareEndpoint,
    SubscribeCtx, SubscribeEndpoint, SystemAudioEndpoint, TargetKind,
};
pub use stross_proto::message::Platform;

/// 应用契约层（壳层只读；展示视图 + 控制面载荷，定义单一真源在
/// stross-types crate，此处重导出保持 `stross_kernel::*` 路径兼容）。
pub use stross_types::{
    AppInfo, AuthorizedView, CameraDevice, CaptureStatusView, DeviceList, EndpointListPayload,
    FilePublishedView, GrantResponseView, IssuedShareTokenView, LocalCatalog,
    PendingRequestsPayload, RelayInfo, SessionCreatedView, SessionView, SessionsPayload,
    ShareTokenView, StartResult, StatusView, StoppedView, StreamStatus, TeardownView,
    UnpublishedView,
};
pub use subscriber::{
    MediaSubscribeOutcome, SubscribeOutcome, fetch_directory, subscribe_file,
    subscribe_file_via_endpoint, subscribe_media, subscribe_media_and_watch,
};
/// 展示视图构造帮助函数（`relay_info` / `watch_urls`；兼容重导出见 [`view`]）。
pub use view::{relay_info, watch_urls};

/// SRT/QUIC 固定传输端口（权限自动化：防火墙只放行已知端口）。真源在
/// [`stross_types::ports`]，此处仅别名保持路径兼容。
pub use stross_types::ports::{QUIC as DEFAULT_QUIC_PORT, SRT as DEFAULT_SRT_PORT};
