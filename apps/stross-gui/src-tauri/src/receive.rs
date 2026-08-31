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
/// 当前 Android 播放链（`spawn_android_playback` 的 blocking 任务句柄）。
///
/// 单条链 = 一个 Kotlin `PlaybackPlugin`（MediaCodec/AudioTrack）。连续订阅
/// 两个媒体端点（如屏幕 → 系统声音）时，若上一条播放链尚未收尾就启动新链，
/// 两条链会在**同一个** Kotlin 插件上竞态（startPlayback/stopPlayback 交叠）→
/// 解码器/音频状态冲突 → 原生崩溃（真机复现：屏幕+声音同时订阅即崩）。
/// 因此 Android 保持**单链路**：`start_receive` 在启新链前先停旧接收并
/// **等待旧链收尾**（多链路 API `start_receive_link` 在 Android 上返回
/// 明确错误——多端点链接为桌面路径，通信模式 v2 Phase C）。
#[cfg(target_os = "android")]
static ANDROID_PLAYBACK: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>> =
    std::sync::Mutex::new(None);

/// 桌面多链路接收：开始接收并登记为链路 `linkId`，解码帧经 `receive-frame`
/// 事件（载荷含 `linkId`）推给前端——多条链路（屏幕 + 系统声音同播）并存，
/// 每条链独立启停/统计（通信模式 v2 Phase C「接收端多流化」）。
/// Android 播放链为单链（MediaCodec 插件竞态），本命令在 Android 返回明确错误。
#[tauri::command]
pub async fn start_receive_link(
    app: tauri::AppHandle,
    state: State<'_, Arc<Kernel>>,
    link_id: String,
    relay: String,
    stream: String,
    audio: stross_endpoint::playback::AudioOut,
) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        let _ = (app, state, link_id, relay, stream, audio);
        return Err("Android 播放链为单链：多端点链接暂仅桌面（通信模式 v2 Phase C）".into());
    }
    #[cfg(not(target_os = "android"))]
    {
        state
            .start_receive_link(link_id.clone(), relay, stream, audio)
            .await
            .map_err(|e| e.to_user_string())?;
        let mut frames = match state.take_receive_frames_for(&link_id) {
            Some(r) => r,
            None => return Err("接收链路已启动但没有帧通道".into()),
        };
        // 帧转发：RGBA 双线性缩放（端点层 `rgba_scaled`，计算在 Rust）到
        // 宽度 ≤ 720 → 事件（显示可跳帧，不反压）。载荷统一为 base64 字符串
        // （桌面/Android 同格式）；`linkId` 供前端多链路路由到对应画布/统计。
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
                    serde_json::json!({
                        "linkId": link_id,
                        "pts": f.pts_ms,
                        "width": w,
                        "height": h,
                        "data": data,
                    }),
                );
            }
        });
        Ok(())
    }
}

/// 停止指定链路（其它链路不受影响；多端点链接）。
#[tauri::command]
pub fn stop_receive_link(state: State<'_, Arc<Kernel>>, link_id: String) {
    state.stop_receive_link(&link_id);
}

/// 全部接收链路快照（linkId + 统计；前端面板逐条展示）。
#[tauri::command]
pub fn receive_links(state: State<'_, Arc<Kernel>>) -> Vec<stross_kernel::receiver::ReceiveLinkView> {
    state.receive_links()
}

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
        // 1) 停旧接收：关闭旧编码帧通道 → 旧播放链循环结束并调用 stopPlayback。
        state.stop_receive();
        // 2) 等旧播放链收尾（stopPlayback 完成）再启新链——消灭同插件竞态。
        //    先取出句柄再 await：否则 `if let` 内临时 MutexGuard 跨 await 持有
        //    （非 Send）→ 命令 future 不满足 tauri 的 Send 约束，编译失败。
        let prev_handle = ANDROID_PLAYBACK.lock().unwrap().take();
        if let Some(prev) = prev_handle {
            let _ = prev.await;
        }
        // 3) 启新接收会话 + 新播放链，句柄入 static 供下次序列化。
        state
            .start_receive_raw(relay.clone(), stream.clone())
            .await
            .map_err(|e| e.to_user_string())?;
        let frames = match state.take_receive_raw_frames() {
            Some(r) => r,
            None => return Err("接收会话已启动但没有编码帧通道".into()),
        };
        let h = crate::mobile::spawn_android_playback(&app, frames, audio);
        *ANDROID_PLAYBACK.lock().unwrap() = Some(h);
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
        // `linkId` 恒为 `main`（旧单流兼容槽；前端多链路路由统一按 linkId）。
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
                    serde_json::json!({
                        "linkId": "main",
                        "pts": f.pts_ms,
                        "width": w,
                        "height": h,
                        "data": data,
                    }),
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
