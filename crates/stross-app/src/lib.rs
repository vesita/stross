//! # stross-app —— 核心封装模块
//!
//! 把共享模块（[`stross_core`]）与系统适配模块（[`stross_media`]）组合成
//! 应用级逻辑，向上层 UI（Tauri 桌面 / Android）暴露一个稳定命令面：
//!
//! * [`engine::SenderEngine`]：推流引擎（中继 + 推流客户端 + 采集后端）
//! * [`app::StrossApp`]：应用状态机（连接 → 推流/观看、mDNS 发现、状态查询）
//!
//! 本模块不依赖任何 UI 框架，可独立单元测试。

pub mod app;
pub mod engine;

pub use app::{CaptureStatusView, Platform, StrossApp};
pub use engine::SenderEngine;
