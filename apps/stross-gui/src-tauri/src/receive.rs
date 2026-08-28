//! GUI 接收播放域（Tauri 命令）：WS 收流 → 解码 → 帧转发前端（桌面 canvas /
//! Android MediaCodec）。壳层职责：仅做**显示路径**粘合（缩放 / base64 / 事件），
//! 流接收本身走 `stross_kernel::Receiver`。

use std::sync::Arc;

use stross_kernel::Kernel;
use tauri::State;
// 桌面接收路径 emit base64 帧载荷需要（Android 走 mobile_jni，不经此模块）。
#[cfg(not(target_os = "android"))]
use base64::Engine as _;
#[cfg(not(target_os = "android"))]
use tauri::Emitter;

/// 开始接收 `relay` 上的 `stream`，解码帧缩放后经 `receive-frame` 事件推到前端。
/// `audio` 决定音频去向：`device` 扬声器播放 / `discard` 静音。
///
/// 平台差异（1f-3）：桌面用 ffmpeg 子进程解码（PlaybackSink）；Android 无
/// ffmpeg，走编码帧转发 → Kotlin MediaCodec 解码（`mobile::spawn_android_playback`），
/// 前端事件与绘制完全一致。
#[tauri::command]
pub async fn start_receive(
    app: tauri::AppHandle,
    state: State<'_, Arc<Kernel>>,
    relay: String,
    stream: String,
    audio: stross_media::playback::AudioOut,
) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        state
            .start_receive_raw(relay.clone(), stream.clone())
            .await
            .map_err(|e| e.to_user_string())?;
        let frames = match state.take_receive_raw_frames() {
            Some(r) => r,
            None => return Err("接收会话已启动但没有编码帧通道".into()),
        };
        crate::mobile::spawn_android_playback(&app, frames, audio);
        Ok(())
    }
    #[cfg(not(target_os = "android"))]
    {
        state
            .start_receive(relay, stream, audio)
            .await
            .map_err(|e| e.to_user_string())?;
        let mut frames = match state.take_receive_frames() {
            Some(r) => r,
            None => return Err("接收会话已启动但没有帧通道".into()),
        };
        // 帧转发：RGBA 最近邻缩放到宽度 ≤ 480 → 事件（显示可跳帧，不反压）。
        // 载荷统一为 base64 字符串（桌面/Android 同格式）：serde 直序列化
        // Vec<u8> 会输出每字节一个数字的 JSON 数组（480×270×4 ≈ 51.8 万元素，
        // ~2.5MB/帧），base64 字符串 ~4 倍紧凑且前端 atob 原生解码。
        tokio::spawn(async move {
            while let Some(f) = frames.recv().await {
                let (w, h, data) = scale_rgba(&f.rgba, f.width, f.height, 480);
                let data = base64::engine::general_purpose::STANDARD.encode(data);
                let _ = app.emit(
                    "receive-frame",
                    serde_json::json!({ "pts": f.pts_ms, "width": w, "height": h, "data": data }),
                );
            }
        });
        Ok(())
    }
}

/// 停止接收。
#[tauri::command]
pub fn stop_receive(state: State<'_, Arc<Kernel>>) {
    state.stop_receive();
}

/// 接收统计（帧数 / 解码 / 音频块）。
#[tauri::command]
pub fn receive_status(state: State<'_, Arc<Kernel>>) -> stross_kernel::ReceiveStats {
    state.receive_status()
}

/// RGBA 最近邻缩放（显示用；保持宽高比，宽度 ≤ `max_w`）。
#[cfg(not(target_os = "android"))]
fn scale_rgba(src: &[u8], w: u32, h: u32, max_w: u32) -> (u32, u32, Vec<u8>) {
    let tw = w.min(max_w);
    let th = (h * tw / w).max(1);
    let mut out = Vec::with_capacity((tw * th * 4) as usize);
    for y in 0..th {
        let sy = (y * h / th) as usize;
        for x in 0..tw {
            let sx = (x * w / tw) as usize;
            let si = (sy * w as usize + sx) * 4;
            out.extend_from_slice(&src[si..si + 4]);
        }
    }
    (tw, th, out)
}

#[cfg(test)]
mod tests {
    use super::scale_rgba;

    #[test]
    fn scale_rgba_keeps_aspect_and_size() {
        // 1280x720 → 宽度上限 480 → 480x270
        let src = vec![0u8; 1280 * 720 * 4];
        let (w, h, out) = scale_rgba(&src, 1280, 720, 480);
        assert_eq!((w, h), (480, 270));
        assert_eq!(out.len(), 480 * 270 * 4);
        // 不超过上限时原样
        let (w2, h2, out2) = scale_rgba(&src, 320, 240, 480);
        assert_eq!((w2, h2), (320, 240));
        assert_eq!(out2.len(), 320 * 240 * 4);
        // 像素值按最近邻拷贝（抽查四角）
        let tiny = vec![0u8; 2 * 2 * 4];
        let (_, _, out3) = scale_rgba(&tiny, 2, 2, 4);
        assert_eq!(out3, tiny);
    }
}
