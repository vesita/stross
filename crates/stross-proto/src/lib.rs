//! # stross-proto
//!
//! 定义 Stross 的线上协议：
//!
//! * **媒体帧**（二进制 WebSocket 消息）：固定 16 字节头 + 载荷。
//! * **控制消息**（文本 WebSocket 消息）：JSON，见 [`message`](crate::message)。

pub mod frame;
pub mod message;

pub use frame::*;
pub use message::*;
