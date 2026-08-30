//! Windows 屏幕采集：ffmpeg gdigrab（无会话变量依赖）。

use std::sync::Arc;

use crate::contract::Probe;
use crate::pipeline::ffmpeg_available;

/// Windows 屏幕端点探测：仅依赖 ffmpeg 可用（gdigrab 无需 DISPLAY）。
pub fn screen_probe() -> Probe {
    Arc::new(|| {
        if ffmpeg_available() {
            Ok(())
        } else {
            Err("ffmpeg 不可用（未安装或 STROSS_FFMPEG 无效）".into())
        }
    })
}

/// 音频类端点探测（麦克风 / 系统声音）：采集后端依赖 ffmpeg。
pub fn audio_probe(label: &'static str) -> Probe {
    Arc::new(move || {
        if ffmpeg_available() {
            Ok(())
        } else {
            Err(format!("ffmpeg 不可用（{label} 采集依赖）"))
        }
    })
}
