//! # stross-core —— 核心局域网共享模块
//!
//! 只负责**数据共享逻辑**，不含任何采集/设备/平台适配：
//!
//! * [`relay`]：中继服务器（推流端 → 中继 → 观看端，WebSocket 广播）
//! * [`sender`]：WS 推流客户端（[`RelayClient`]）
//! * [`discovery`]：mDNS 服务发现（feature `discovery`）
//! * [`net`]：本机局域网 IP
//! * [`assets`]：内嵌观看端页面
//!
//! 架构：**推流端 → 中继 → 观看端**（协议定义见 [`stross_proto`]）：
//!
//! ```text
//! +-----------+   raw ES (H.264/AAC)   +--------+   raw ES      +---------+
//! | 推流端     | ── WebSocket push ───▶ │ 中继    │ ── broadcast ▶ │ 观看端    |
//! | (media)   |                        | (relay) |               | (MSE 播放) │
//! +-----------+                        +--------+               +---------+
//! ```

pub mod assets;
#[cfg(feature = "discovery")]
pub mod discovery;
pub mod net;
pub mod relay;
pub mod sender;

pub use relay::{RelayHandle, RelayServer};
pub use sender::RelayClient;
