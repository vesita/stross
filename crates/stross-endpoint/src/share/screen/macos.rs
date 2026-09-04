//! macOS 屏幕与音频采集：基于 ffmpeg avfoundation 引擎。

use std::sync::Arc;

use crate::contract::Probe;
use crate::pipeline::ffmpeg_available;

/// macOS 屏幕端点探测：依赖 ffmpeg 可用（支持 avfoundation 屏幕采集）。
pub fn screen_probe() -> Probe {
    Arc::new(|| {
        if ffmpeg_available() {
            Ok(())
        } else {
            Err(
                "ffmpeg 不可用（未安装或 STROSS_FFMPEG 无效，macOS 屏幕采集依赖 avfoundation）"
                    .into(),
            )
        }
    })
}

/// 音频类端点探测（麦克风 / 系统声音）：采集后端依赖 ffmpeg avfoundation。
pub fn audio_probe(label: &'static str) -> Probe {
    Arc::new(move || {
        if ffmpeg_available() {
            Ok(())
        } else {
            Err(format!("ffmpeg 不可用（{label} 采集依赖）"))
        }
    })
}
