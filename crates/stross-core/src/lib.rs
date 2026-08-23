//! # stross-core
//!
//! Stross 的核心库：采集管线（ffmpeg 编排）、中继服务器（axum + WebSocket）、
//! 设备枚举、mDNS 发现与网络工具。
//!
//! 架构：**推流端 → 中继 → 观看端**
//!
//! ```text
//! +-----------+   raw ES (H.264/AAC)   +--------+   raw ES      +---------+
//! | 推流端     | ── WebSocket push ───▶ │ 中继    │ ── broadcast ▶ │ 观看端    │
//! | (ffmpeg)  |                        | (relay) |               | (MSE 播放) │
//! +-----------+                        +--------+               +---------+
//! ```

pub mod adts;
pub mod assets;
pub mod devices;
#[cfg(feature = "discovery")]
pub mod discovery;
pub mod nal;
pub mod net;
pub mod pipeline;
pub mod relay;
pub mod sender;

pub use pipeline::{AudioSourceConfig, Quality, StreamConfig, StreamSession, VideoSource};
pub use relay::{RelayHandle, RelayServer};
pub use sender::SenderEngine;
