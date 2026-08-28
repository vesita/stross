//! Linux 屏幕采集：探测与后端路由。
//!
//! 通用性优先（不做 GPU 厂商判断）：
//!
//! * **Wayland 会话**（`WAYLAND_DISPLAY` 设置）：走 [`super::wayland`] 的
//!   xdg-desktop-portal ScreenCast + PipeWire **CPU/SHM 路径**——合成器无关
//!   （KWin / Mutter / wlroots 全部支持），不依赖任何显卡厂商特性；
//!   这也是避坑选择：DMA-BUF linear mmap 在部分 GPU（如 AMD tiled 内存）上
//!   读回全零，SHM 路径无此问题。
//! * **X11 会话**（`DISPLAY` 设置，无 Wayland）：ffmpeg x11grab（沿用既有路径）。

use std::sync::Arc;

use crate::contract::Probe;
use crate::pipeline::ffmpeg_available;

/// 当前会话是否为 Wayland。
pub fn is_wayland_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// ffmpeg 是否可用（采集编码后端依赖；内核 `app_info` 同源）。
fn ffmpeg_ok() -> bool {
    ffmpeg_available()
}

/// Linux 屏幕端点探测：ffmpeg 可用 + 图形会话存在。
///
/// * Wayland：`WAYLAND_DISPLAY` 设置即可（portal 授权在 share 时由
///   系统对话框完成；拒绝/失败经 CaptureStatus.error 回报）
/// * X11：`DISPLAY` 设置（x11grab 依赖）
///
/// 无图形会话 → 标记不可挂载，UI/目录直接可见原因。
pub fn screen_probe() -> Probe {
    Arc::new(|| {
        if !ffmpeg_ok() {
            return Err("ffmpeg 不可用（未安装或 STROSS_FFMPEG 无效）".into());
        }
        let has_display = std::env::var_os("DISPLAY").is_some();
        let has_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        if !has_display && !has_wayland {
            return Err("无图形会话（DISPLAY / WAYLAND_DISPLAY 均未设置）".into());
        }
        Ok(())
    })
}

/// 音频类端点探测（麦克风 / 系统声音）：采集后端依赖 ffmpeg。
pub fn audio_probe(label: &'static str) -> Probe {
    Arc::new(move || {
        if ffmpeg_ok() {
            Ok(())
        } else {
            Err(format!("ffmpeg 不可用（{label} 采集依赖）"))
        }
    })
}
