//! 订阅端点（内容宿）目录：消费对端流并还原（播放器 / 文件接收）。
//!
//! docs/endpoint-model-v2.md §3 演进：订阅端与分享端是**独立契约**（内核
//! 约定特性、端点实现、内核只基于特性行动）——本目录是订阅端的实现区，
//! 不承载任何采集/分享逻辑（那是 [`crate::share`] 的职责）。
//!
//! * [`media`]：Graph / Audio 能力族的统一订阅端（收流 + 解码，播放器入端点）
//! * [`file`]：File 能力族的订阅端（接收落盘）

pub mod channel;
pub mod file;
pub mod media;

pub use channel::FileChannelSubscribeEndpoint;
pub use file::FileReceiveEndpoint;
pub use media::MediaReceiveEndpoint;
