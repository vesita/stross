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

/// 桌面回传帧最大宽度。双线性缩放下 720p 显示够用（全屏放大仍清晰），
/// 且 720×405 RGBA ≈ 1.2MB/帧（≈ 2.5× 原 480 上限）控制 IPC 流量；
/// Android 走 `mobile_jni::MAX_FRAME_W = 480`（小屏 + WebView IPC 弱）。
#[cfg(not(target_os = "android"))]
const RECV_MAX_W: u32 = 720;

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
    audio: stross_endpoint::playback::AudioOut,
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
        // 帧转发：RGBA 双线性缩放（端点层 `rgba_scaled`，计算在 Rust）到
        // 宽度 ≤ 720 → 事件（显示可跳帧，不反压）。
        // 载荷统一为 base64 字符串（桌面/Android 同格式）：serde 直序列化
        // Vec<u8> 会输出每字节一个数字的 JSON 数组（720×405×4 ≈ 116 万元素，
        // ~5.7MB/帧），base64 字符串 ~4 倍紧凑且前端 atob 原生解码。
        tokio::spawn(async move {
            while let Some(f) = frames.recv().await {
                let Some((w, h, data)) =
                    stross_endpoint::rgba_scaled(&f.rgba, f.width, f.height, RECV_MAX_W)
                else {
                    continue;
                };
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
