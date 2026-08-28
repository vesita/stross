//! macOS 屏幕采集：暂不支持（probe 直接不可挂载，原因前置）。

use std::sync::Arc;

use crate::contract::Probe;

/// macOS 屏幕端点探测：明确标记不可挂载（避免共享时才炸）。
pub fn screen_probe() -> Probe {
    Arc::new(|| Err("macOS 屏幕采集暂不支持（请使用原生采集路径）".into()))
}

/// 音频类端点探测（麦克风 / 系统声音）：采集后端依赖 ffmpeg。
pub fn audio_probe(label: &'static str) -> Probe {
    Arc::new(move || Err(format!("macOS 音频采集暂不支持（{label}）")))
}
