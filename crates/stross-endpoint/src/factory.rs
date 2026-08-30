//! 平台端点装配：构造默认端点集 + 注入内核（端点插件区的出厂清单）。
//!
//! 平台知识只在这里（原 bridge/devices.rs 收敛到端点层）：`cfg(target_os)`
//! 分支、探测闭包构造都在本模块；内核零 OS 调用、壳层零端点清单。
//!
//! 本模块不依赖内核类型：经 [`EndpointSeeder`] 契约把端点注入内核注册表
//! （内核实现该契约；端点层只依赖 stross-proto）。

use stross_proto::message::{MediaKind, Platform};

use crate::contract::ShareEndpoint;
use crate::share::audio::{MicEndpoint, SystemAudioEndpoint};
use crate::share::screen::ScreenEndpoint;

/// 端点注入目标（内核实现）：登记端点 + 查询平台。端点层不依赖内核类型。
pub trait EndpointSeeder {
    /// 登记分享端点并立即 load 探测（幂等：按端点 id 去重）。
    fn seed_endpoint(&self, ep: Box<dyn ShareEndpoint>) -> bool;
    /// 当前运行平台（内核经 bridge 注入判定）。
    fn platform(&self) -> Platform;
}

/// Android 音频端点探测：采集走原生（MediaRecorder / AAudio），不依赖 ffmpeg，
/// 恒可用（音频采集能力由实际设备决定，load 不额外门控）。
#[cfg(target_os = "android")]
fn android_audio_probe() -> crate::contract::Probe {
    std::sync::Arc::new(|| Ok(()))
}

/// 平台端点构造（camera 按采集能力后置）。桌面与 Android 都是
/// 屏幕 + 麦克风 + 系统声音三件套：
///
/// * 桌面屏幕探测 = 图形会话 + ffmpeg（无 DISPLAY/WAYLAND 前置化为不可挂载）；
/// * Android 屏幕 = MediaProjection（采集执行在壳层注入的采集后端，运行时
///   授权异步回报，探测恒可用，见 [`crate::share::screen::android`]）。
pub fn platform_endpoints(platform: Platform) -> Vec<Box<dyn ShareEndpoint>> {
    #[cfg(target_os = "linux")]
    let probes = (
        crate::share::screen::linux::screen_probe(),
        crate::share::screen::linux::audio_probe("麦克风"),
        crate::share::screen::linux::audio_probe("系统声音"),
    );
    #[cfg(target_os = "windows")]
    let probes = (
        crate::share::screen::windows::screen_probe(),
        crate::share::screen::windows::audio_probe("麦克风"),
        crate::share::screen::windows::audio_probe("系统声音"),
    );
    #[cfg(target_os = "macos")]
    let probes = (
        crate::share::screen::macos::screen_probe(),
        crate::share::screen::macos::audio_probe("麦克风"),
        crate::share::screen::macos::audio_probe("系统声音"),
    );
    #[cfg(target_os = "android")]
    let probes = (
        crate::share::screen::android::screen_probe(),
        android_audio_probe(),
        android_audio_probe(),
    );

    let mut v: Vec<Box<dyn ShareEndpoint>> = vec![
        Box::new(ScreenEndpoint::new("屏幕", probes.0)),
        Box::new(MicEndpoint::new("麦克风", probes.1)),
        Box::new(SystemAudioEndpoint::new("系统声音", probes.2)),
    ];

    if matches!(platform, Platform::Android) {
        // Android 暂不构造摄像头端点（采集能力后置）；屏幕（MediaProjection）
        // 已随三件套构造
        v.retain(|e| e.kind() != MediaKind::Camera);
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
    fn android_has_screen_audio_trio() {
        let list = platform_endpoints(Platform::Android);
        assert!(
            list.iter().any(|e| e.kind() == MediaKind::Screen),
            "Android 屏幕端点（MediaProjection）应构造"
        );
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
