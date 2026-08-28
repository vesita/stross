//! # stross-kernel —— Stross 内核
//!
//! **内核 = 全部平台无关的服务提供**。数据面、信令面、端点框架、发现、
//! 推流/接收编排都在这里，以 [`Kernel`] 门面为单一入口：
//!
//! * [`relay`]：中继服务器 + 中继 HTTP 客户端（数据面，服务端契约单一真源）
//! * [`sender`] / [`watch`] / [`jitter`] / [`session_channel`]：推流客户端与观看链路
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
pub mod devices;
#[cfg(feature = "discovery")]
pub mod discovery;
pub mod endpoint_driver;
pub mod engine;
pub mod error;
pub mod file_xfer;
pub mod jitter;
pub mod kernel;
mod lock;
pub mod negotiator;
pub mod negotiator_client;
pub mod platform;
pub mod receiver;
pub mod relay;
pub mod sender;
pub mod session_channel;
pub mod subscriber;
pub mod view;
pub mod watch;

// 传输层（独立 crate stross-transport）：
// `stross_kernel::transport::*` 与 `stross_kernel::net::*` 路径保持兼容。
pub use stross_transport as transport;
pub use stross_transport::net;
pub use stross_transport::{DataSession, SessionPacket, Transport, TransportError, TransportStats};

pub use control::{CtrlRequest, CtrlResponse, CtrlServer, DEFAULT_CTRL_PORT};
pub use devices::{ScannedDevice, StreamView, probe_base, scan, scan_lan, to_views};
pub use engine::SenderEngine;
pub use error::{Error, RelayOpError, Result, WatchError};
pub use file_xfer::{FilePushOptions, ReceivedFile, receive_file, receive_file_session};
pub use kernel::{
    AuthError, AuthPolicy, DataPlaneBackend, EndpointRegistry, FileSource, Kernel, KernelEvent,
    NodeInfo, NodeRole, PinAuthPolicy, RelayDataPlane, Session, SessionPrefs, SubscribeCtx,
    SubscribeHook, TransportAddr,
};
pub use negotiator::{
    CliUi, DEFAULT_NEGOTIATOR_PORT, DeviceIdentity, NegotiatorUi, NoopUi, PendingRequest,
    RelayAddr, ShareGrant, ShareNegotiator, ShareRequest, TrustStore, load_or_create_identity,
};
pub use negotiator_client::request_grant;
pub use platform::Platform;
pub use receiver::{ReceiveStats, Receiver};
pub use relay::{DEFAULT_PORT, GUI_PORT, RelayHandle, RelayServer};
pub use sender::RelayClient;
pub use subscriber::{
    MediaSubscribeOutcome, SubscribeOutcome, fetch_directory, subscribe_file, subscribe_media,
};
pub use view::{
    AppInfo, CaptureStatusView, DeviceList, LocalCatalog, RelayInfo, ShareTokenView, StartResult,
    StreamStatus,
};

/// SRT/QUIC 固定传输端口（权限自动化：防火墙只放行已知端口）。
pub const DEFAULT_SRT_PORT: u16 = 33462;
pub const DEFAULT_QUIC_PORT: u16 = 33464;
