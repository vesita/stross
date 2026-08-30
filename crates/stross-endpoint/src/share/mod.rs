//! 分享端点（内容源）目录：把本机媒体/数据变成流，可被订阅。
//!
//! docs/endpoint-model-v2.md §3 演进：分享端与订阅端是**独立契约**（内核
//! 约定特性、端点实现、内核只基于特性行动）——本目录是分享端的实现区，
//! 不承载任何播放/接收逻辑（那是 [`crate::subscribe`] 的职责）。
//!
//! * [`audio`]：麦克风 / 系统声音（Audio 能力族）
//! * [`screen`]：屏幕（Graph 能力族；Linux/Wayland、Windows、macOS、Android）
//! * [`file`]：文件（File 能力族，确定目标）

pub mod audio;
pub mod file;
pub mod screen;

pub use audio::{MicEndpoint, SystemAudioEndpoint};
pub use file::{FileEndpoint, FilePushOptions};
pub use screen::ScreenEndpoint;
