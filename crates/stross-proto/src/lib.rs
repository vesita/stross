//! # stross-proto
//!
//! 定义 Stross 的线上协议：
//!
//! * **媒体帧**（二进制 WebSocket 消息）：固定 24 字节头（v2，含帧序号与分片字段）+ 载荷。
//! * **控制消息**（文本 WebSocket 消息）：JSON，见 [`message`](crate::message)，
//!   含能力协商（`Capabilities`/`Offer`/`Answer`）与路由控制（`Route`）。

pub mod frame;
pub mod message;

pub use frame::*;
pub use message::*;
