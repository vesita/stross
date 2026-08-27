//! # stross-app —— 核心封装模块
//!
//! 把共享模块（[`stross_core`]）与系统适配模块（[`stross_media`]）组合成
//! 应用级逻辑，向上层 UI（Tauri 桌面 / Android）暴露一个稳定命令面：
//!
//! * [`engine::SenderEngine`]：推流引擎（中继 + 推流客户端 + 采集后端）
//! * [`app::StrossApp`]：应用状态机（连接 → 推流/观看、mDNS 发现、状态查询）
//! * [`kernel::Kernel`]：内核（控制面）——设备图 / 会话管理 / 路由
//!   （设计文档 docs/plugin-architecture.md §3；阶段 0 提供路由 API）
//! * [`control::CtrlServer`]：控制面端点（D7）——CLI 可接入异步控制
//!   （仅回环绑定，见 docs/requirements.md D7）
//!
//! 本模块不依赖任何 UI 框架，可独立单元测试。

pub mod app;
pub mod bootstrap;
pub mod control;
pub mod endpoint_driver;
pub mod engine;
pub mod error;
pub mod file_xfer;
pub mod kernel;
mod lock;
pub mod negotiator;
pub mod paths;
pub mod receiver;
pub mod subscriber;

pub use app::{CaptureStatusView, Platform, StrossApp};
pub use control::{CtrlRequest, CtrlResponse, CtrlServer, DEFAULT_CTRL_PORT};
pub use endpoint_driver::install_endpoint_driver;
pub use engine::SenderEngine;
pub use error::{Error, Result};
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
pub use receiver::{ReceiveStats, Receiver};
pub use subscriber::{SubscribeOutcome, fetch_directory, subscribe_file};

/// SRT/QUIC 固定传输端口（权限自动化：防火墙只放行已知端口）。
pub const DEFAULT_SRT_PORT: u16 = 33462;
pub const DEFAULT_QUIC_PORT: u16 = 33464;
