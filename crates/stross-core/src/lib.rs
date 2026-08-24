//! # stross-core —— 核心局域网共享模块
//!
//! 只负责**数据共享逻辑**，不含任何采集/设备/平台适配：
//!
//! * [`relay`]：中继服务器（推流端 → 中继 → 接收端，基于传输抽象广播）
//! * [`sender`]：推流客户端（[`RelayClient`]，基于传输抽象拨号）
//! * [`transport`]：可插拔传输层（[`Transport`]/[`DataSession`]；独立 crate
//!   [`stross_transport`]，此处 re-export 保持路径兼容）
//! * [`discovery`]：mDNS 服务发现（feature `discovery`）
//! * [`net`]：本机局域网 IP（传输层提供，此处 re-export）
//!
//! 架构：**推流端 → 中继 → 接收端**（协议定义见 [`stross_proto`]）：
//!
//! ```text
//! +-----------+   raw ES (H.264/AAC)   +--------+   raw ES      +---------+
//! | 推流端     | ── Transport push ───▶ │ 中继    │ ── broadcast ▶ │ 接收端    |
//! | (media)   |                        | (relay) |               | (原生播放) │
//! +-----------+                        +--------+               +---------+
//! ```
//!

#[cfg(feature = "discovery")]
pub mod discovery;
pub mod jitter;
pub mod relay;
pub mod sender;
pub mod session_channel;

// 传输层（独立 crate stross-transport，阶段 2 拆分）：
// `stross_core::transport::*` 与 `stross_core::net::*` 路径保持兼容。
pub use stross_transport as transport;
pub use stross_transport::net;
pub use stross_transport::{DataSession, SessionPacket, Transport, TransportError, TransportStats};

pub use relay::{RelayHandle, RelayServer};
pub use sender::RelayClient;
