//! Android 屏幕端点探测（MediaProjection 路径）。
//!
//! Android 屏幕采集 = MediaProjection（系统投影）+ 前台服务（FGS 授权）：
//!
//! * **采集执行**在壳层注入的 [`CaptureBackend`]（`AndroidCapture`，MediaProjection
//!   + MediaCodec 编码）——端点 `share` 只组 [`StreamConfig`]（`VideoSource::Screen`）
//!   调内核调度，与桌面完全同构，零平台分支；
//! * **探测**只判断「平台能力是否存在」：MediaProjection 自 API 21 起恒可用、
//!   前台服务静态声明在 manifest——因此恒 `Ok`；**运行时授权**（系统弹窗 /
//!   用户拒绝 / FGS 未启动）由采集后端经 [`CaptureStatus`] 异步回报，UI 层展示，
//!   不属 load 探测范畴（与桌面「无图形会话」前置化为 load 失败不同——
//!   桌面没有权限弹窗，授权模型不同）。
//!
//! 分层：本模块只产探测闭包（平台知识收敛点）；`cfg(target_os)` 分支只允许
//! 出现在本目录与 [`crate::factory`]。

use crate::contract::Probe;

/// Android 屏幕采集可用性探测：恒可用（平台能力静态存在；运行时授权由
/// 采集后端异步回报，见模块文档）。
pub fn screen_probe() -> Probe {
    std::sync::Arc::new(|| Ok(()))
}
