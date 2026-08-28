//! 平台设备静态枚举（docs/endpoint-model.md §11）与注入。
//!
//! 设备 = 持久能力实体（摄像头 / 麦克风 / 屏幕），与订阅与否无关；被公开后
//! 才实例化为端点。**平台知识只在这里**：内核不出现 `target_os` 分支，
//! 壳层也不必再各写一份设备清单。

use stross_kernel::Platform;
use stross_proto::message::{DeviceInfo, MediaKind};

/// 当前运行平台（`cfg(target_os="android")` 判定只允许出现在这里）。
pub fn platform() -> Platform {
    if cfg!(target_os = "android") {
        Platform::Android
    } else {
        Platform::Desktop
    }
}

/// 平台设备静态枚举（camera 按采集能力后置；Android P1 不默认公开屏幕——
/// 依赖前台服务权限，micOnly 路径已验证，屏幕采集权限后置）。
pub fn platform_devices(platform: Platform) -> Vec<DeviceInfo> {
    let mut v = vec![
        DeviceInfo {
            device_id: "screen:0".into(),
            kind: MediaKind::Screen,
            name: "屏幕".into(),
            builtin: true,
        },
        DeviceInfo {
            device_id: "mic:builtin".into(),
            kind: MediaKind::Mic,
            name: "麦克风".into(),
            builtin: true,
        },
        DeviceInfo {
            device_id: "sysaudio:builtin".into(),
            kind: MediaKind::SystemAudio,
            name: "系统声音".into(),
            builtin: true,
        },
    ];
    if matches!(platform, Platform::Android) {
        v.retain(|d| d.kind != MediaKind::Screen);
    }
    v
}

/// 把当前平台设备清单注入内核（幂等：重复注册按 device_id 去重）。
///
/// 启动原语：CLI serve 与 GUI 桌面 / Android 都只调这一个入口，
/// 不再各写一份设备枚举。
pub fn seed_platform_devices(kernel: &stross_kernel::Kernel) {
    for d in platform_devices(kernel.platform()) {
        kernel.seed_device(d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn android_excludes_screen() {
        let list = platform_devices(Platform::Android);
        assert!(list.iter().all(|d| d.kind != MediaKind::Screen));
        assert!(list.iter().any(|d| d.kind == MediaKind::Mic));
    }

    #[test]
    fn desktop_has_screen() {
        let list = platform_devices(Platform::Desktop);
        assert!(list.iter().any(|d| d.kind == MediaKind::Screen));
    }
}
