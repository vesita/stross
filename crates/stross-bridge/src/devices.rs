//! 平台端点构造与注入（docs/endpoint-model.md §1：端点 = 可共享能力实体）。
//!
//! 端点 = 屏幕 / 麦克风 / 系统声音等能力实体，各自实现 `load`（探测自身
//! 可用性，能否被挂载成节点）与 `share`（订阅达成后推流）契约。**平台知识
//! 只在这里**：内核不出现 `target_os` 分支，壳层也不必再各写一份端点清单。
//!
//! 屏幕端点的 load 探测（屏幕获取失败**前置化**）：无图形会话（DISPLAY /
//! WAYLAND_DISPLAY 均未设置）或 ffmpeg 不可用 → 标记不可挂载 + 原因，
//! 对端目录可见但不可订阅——不再等订阅后才在推流时炸。

use std::sync::Arc;

use stross_kernel::{Endpoint, MicEndpoint, Platform, Probe, ScreenEndpoint, SystemAudioEndpoint};
use stross_proto::message::MediaKind;

/// 当前运行平台（`cfg(target_os="android")` 判定只允许出现在这里）。
pub fn platform() -> Platform {
    if cfg!(target_os = "android") {
        Platform::Android
    } else {
        Platform::Desktop
    }
}

/// ffmpeg 是否可用（采集后端依赖；内核 `app_info` 同源）。
fn ffmpeg_ok() -> bool {
    stross_media::pipeline::ffmpeg_available()
}

/// 屏幕端点探测：ffmpeg 可用 + 图形会话存在。
///
/// Linux 桌面屏幕采集走 x11grab（需 DISPLAY；Wayland 会话无 XWayland 时
/// 同样不可用）；Windows 走 gdigrab（无需会话变量）。探测失败 = 屏幕端点
/// 标记不可挂载，UI/目录直接可见原因。
pub fn screen_probe() -> Probe {
    Arc::new(|| {
        if !ffmpeg_ok() {
            return Err("ffmpeg 不可用（未安装或 STROSS_FFMPEG 无效）".into());
        }
        #[cfg(target_os = "linux")]
        {
            let has_display = std::env::var_os("DISPLAY").is_some();
            let has_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
            if !has_display && !has_wayland {
                return Err("无图形会话（DISPLAY / WAYLAND_DISPLAY 均未设置）".into());
            }
        }
        Ok(())
    })
}

/// 音频类端点探测（麦克风 / 系统声音）：采集后端依赖 ffmpeg。
fn audio_probe(label: &'static str) -> Probe {
    Arc::new(move || {
        if ffmpeg_ok() {
            Ok(())
        } else {
            Err(format!("ffmpeg 不可用（{label} 采集依赖）"))
        }
    })
}

/// 平台端点构造（camera 按采集能力后置；Android P1 不构造屏幕端点——
/// 依赖前台服务权限，micOnly 路径已验证，屏幕采集权限后置）。
pub fn platform_endpoints(platform: Platform) -> Vec<Box<dyn Endpoint>> {
    let mut v: Vec<Box<dyn Endpoint>> = vec![
        Box::new(ScreenEndpoint::new("屏幕", screen_probe())),
        Box::new(MicEndpoint::new("麦克风", audio_probe("麦克风"))),
        Box::new(SystemAudioEndpoint::new(
            "系统声音",
            audio_probe("系统声音"),
        )),
    ];
    if matches!(platform, Platform::Android) {
        v.retain(|e| e.kind() != MediaKind::Screen);
    }
    v
}

/// 把当前平台端点注入内核（登记 + 立即 load 探测可挂载性；幂等：按端点 id
/// 去重）。
///
/// 启动原语：CLI serve 与 GUI 桌面 / Android 都只调这一个入口，
/// 不再各写一份端点构造。
pub fn seed_platform_endpoints(kernel: &stross_kernel::Kernel) {
    for ep in platform_endpoints(kernel.platform()) {
        kernel.seed_endpoint(ep);
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
    }

    #[test]
    fn desktop_has_screen() {
        let list = platform_endpoints(Platform::Desktop);
        assert!(list.iter().any(|e| e.kind() == MediaKind::Screen));
    }
}
