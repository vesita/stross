//! 平台端点装配（委托给 stross-endpoint 插件区）。
//!
//! 端点构造 / 探测闭包已在 stross-endpoint（screen/audio/file + factory），
//! 本文件只保留**平台判定**（`platform()`，OS 分支收敛点）与对外的
//! `platform_endpoints` / `seed_platform_endpoints` 兼容委托——壳层
//! （CLI/GUI）继续经 `stross_bridge::*` 入口，无感知。

use stross_kernel::Kernel;
use stross_proto::message::Platform;

/// 当前运行平台（`cfg(target_os="android")` 判定只允许出现在这里）。
pub fn platform() -> Platform {
    if cfg!(target_os = "android") {
        Platform::Android
    } else {
        Platform::Desktop
    }
}

/// 平台端点构造（定义与实现单一真源在 stross-endpoint）。
pub fn platform_endpoints(platform: Platform) -> Vec<Box<dyn stross_kernel::Endpoint>> {
    stross_endpoint::factory::platform_endpoints(platform)
}

/// 把当前平台端点注入内核（委托 stross-endpoint 端点装配）。
pub fn seed_platform_endpoints(kernel: &Kernel) {
    stross_endpoint::factory::seed_platform_endpoints(kernel);
}
