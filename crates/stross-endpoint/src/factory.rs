//! 平台端点装配：构造默认端点集 + 注入内核（端点插件区的出厂清单）。
//!
//! 平台知识只在这里（原 bridge/devices.rs 收敛到端点层）：`cfg(target_os)`
//! 分支、探测闭包构造都在本模块；内核零 OS 调用、壳层零端点清单。
//!
//! 本模块不依赖内核类型：经 [`EndpointSeeder`] 契约把端点注入内核注册表
//! （内核实现该契约；端点层只依赖 stross-proto）。

use stross_proto::message::{MediaKind, Platform};

use crate::audio::{MicEndpoint, SystemAudioEndpoint};
use crate::contract::Endpoint;
use crate::screen::ScreenEndpoint;

/// 端点注入目标（内核实现）：登记端点 + 查询平台。端点层不依赖内核类型。
pub trait EndpointSeeder {
    /// 登记端点并立即 load 探测（幂等：按端点 id 去重）。
    fn seed_endpoint(&self, ep: Box<dyn Endpoint>) -> bool;
    /// 当前运行平台（内核经 bridge 注入判定）。
    fn platform(&self) -> Platform;
}

/// 平台端点构造（camera 按采集能力后置；Android P1 不构造屏幕端点——
/// 依赖前台服务权限，micOnly 路径已验证，屏幕采集权限后置）。
pub fn platform_endpoints(platform: Platform) -> Vec<Box<dyn Endpoint>> {
    #[cfg(target_os = "linux")]
    let probes = (
        crate::screen::linux::screen_probe(),
        crate::screen::linux::audio_probe("麦克风"),
        crate::screen::linux::audio_probe("系统声音"),
    );
    #[cfg(target_os = "windows")]
    let probes = (
        crate::screen::windows::screen_probe(),
        crate::screen::windows::audio_probe("麦克风"),
        crate::screen::windows::audio_probe("系统声音"),
    );
    #[cfg(target_os = "macos")]
    let probes = (
        crate::screen::macos::screen_probe(),
        crate::screen::macos::audio_probe("麦克风"),
        crate::screen::macos::audio_probe("系统声音"),
    );
    #[cfg(target_os = "android")]
    let probes = (
        // Android 屏幕端点不构造（见函数头注释）；音频探测仅 ffmpeg 依赖
        crate::screen::linux::audio_probe("麦克风"),
        crate::screen::linux::audio_probe("系统声音"),
    );

    #[cfg(not(target_os = "android"))]
    let mut v: Vec<Box<dyn Endpoint>> = vec![
        Box::new(ScreenEndpoint::new("屏幕", probes.0)),
        Box::new(MicEndpoint::new("麦克风", probes.1)),
        Box::new(SystemAudioEndpoint::new("系统声音", probes.2)),
    ];
    #[cfg(target_os = "android")]
    let mut v: Vec<Box<dyn Endpoint>> = vec![
        Box::new(MicEndpoint::new("麦克风", probes.0)),
        Box::new(SystemAudioEndpoint::new("系统声音", probes.1)),
    ];

    if matches!(platform, Platform::Android) {
        v.retain(|e| e.kind() != MediaKind::Screen);
    }
    v
}

/// 把当前平台端点注入 seeder（内核注册表；登记 + 立即 load 探测）。
///
/// 启动原语：CLI serve 与 GUI 桌面 / Android 都只调这一个入口，
/// 不再各写一份端点构造。
pub fn seed_platform_endpoints(seeder: &dyn EndpointSeeder) {
    for ep in platform_endpoints(seeder.platform()) {
        seeder.seed_endpoint(ep);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn android_excludes_screen() {
        let list = platform_endpoints(Platform::Android);
        assert!(list.iter().all(|e| e.kind() != MediaKind::Screen));
        assert!(list.iter().any(|e| e.kind() == MediaKind::Mic));
        assert!(list.iter().any(|e| e.kind() == MediaKind::SystemAudio));
    }

    #[test]
    fn desktop_has_screen() {
        let list = platform_endpoints(Platform::Desktop);
        assert!(list.iter().any(|e| e.kind() == MediaKind::Screen));
        assert!(list.iter().any(|e| e.kind() == MediaKind::Mic));
        assert!(list.iter().any(|e| e.kind() == MediaKind::SystemAudio));
    }
}
