//! GUI 接收播放域（Tauri 命令）：WS 收流 → 解码 → 帧转发前端（桌面 canvas /
//! Android MediaCodec）。壳层职责：仅做**显示路径**粘合（缩放 / base64 / 事件），
//! 流接收本身走 `stross_kernel::Receiver`。

use std::sync::Arc;

use stross_kernel::Kernel;
use tauri::State;
use tauri::ipc::Channel;

/// 桌面回传帧二进制头（Channel 载荷前缀）：magic + width + height + pts，
/// 各 u32 小端，后接 RGBA 像素（w×h×4）。前端 DataView 解析后零拷贝绘制。
#[cfg(not(target_os = "android"))]
const FRAME_MAGIC: u32 = 0x5354_5246; // "STRF"
#[cfg(not(target_os = "android"))]
fn pack_frame(w: u32, h: u32, pts: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + rgba.len());
    out.extend_from_slice(&FRAME_MAGIC.to_le_bytes());
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes());
    out.extend_from_slice(&pts.to_le_bytes());
    out.extend_from_slice(rgba);
    out
}

#[cfg(all(test, not(target_os = "android")))]
mod tests {
    use super::*;

    /// 二进制帧头布局（前端 DataView 按小端解析，布局回归防护）：
    /// [0..4] magic "STRF"、[4..8] w、[8..12] h、[12..16] pts，后接 RGBA。
    #[test]
    fn pack_frame_header_layout() {
        let rgba = vec![7u8; 720 * 405 * 4];
        let out = pack_frame(720, 405, 1000, &rgba);
        assert_eq!(out.len(), 16 + rgba.len());
        assert_eq!(
            u32::from_le_bytes(out[0..4].try_into().unwrap()),
            FRAME_MAGIC
        );
        assert_eq!(u32::from_le_bytes(out[4..8].try_into().unwrap()), 720);
        assert_eq!(u32::from_le_bytes(out[8..12].try_into().unwrap()), 405);
        assert_eq!(u32::from_le_bytes(out[12..16].try_into().unwrap()), 1000);
        assert_eq!(&out[16..], &rgba[..]);
    }
}

/// 桌面回传帧最大宽度。双线性缩放下 720p 显示够用（全屏放大仍清晰），
/// 且 720×405 RGBA ≈ 1.2MB/帧（≈ 2.5× 原 480 上限）控制 IPC 流量；
/// Android 走 `mobile_jni::MAX_FRAME_W = 480`（小屏 + WebView IPC 弱）。
#[cfg(not(target_os = "android"))]
const RECV_MAX_W: u32 = 720;

/// 开始接收 `relay` 上的 `stream`，解码帧缩放后经 `onFrame` 二进制通道推到前端。
/// `audio` 决定音频去向：`device` 扬声器播放 / `discard` 静音。
///
/// 平台差异（1f-3）：桌面用 ffmpeg 子进程解码（PlaybackSink）；Android 无
/// ffmpeg，走编码帧转发 → Kotlin MediaCodec 解码（`mobile::spawn_android_playback`），
/// 显示帧经 `mobile_jni` 的 `receive-frame` 事件（与桌面不同路径）。
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
static ANDROID_PLAYBACK_LINKS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, tokio::task::JoinHandle<()>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
/// 多链路接收（桌面 + Android 统一支持）：开始接收并登记为链路 `linkId`。
/// * 桌面：解码帧经 `onFrame` 二进制通道直推前端 canvas。
/// * Android：编码帧经 `mobile_jni` 与 `PlaybackPlugin`（MediaCodec 视频 + AudioTrack 音频），支持音画多流并发消费。
#[tauri::command]
pub async fn start_receive_link(
    app: tauri::AppHandle,
    state: State<'_, Arc<Kernel>>,
    link_id: String,
    relay: String,
    stream: String,
    audio: stross_endpoint::playback::AudioOut,
    on_frame: Channel<Vec<u8>>,
) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        let _ = &on_frame;
        state
            .start_receive_raw_link(link_id.clone(), relay, stream)
            .await
            .map_err(|e| e.to_user_string())?;
        let frames = match state.take_receive_raw_frames_for(&link_id) {
            Some(r) => r,
            None => return Err("接收链路已启动但没有编码帧通道".into()),
        };
        let h = crate::mobile::spawn_android_playback(&app, frames, audio);
        let mut guard = ANDROID_PLAYBACK_LINKS.lock().unwrap();
        if let Some(old) = guard.insert(link_id, h) {
            old.abort();
        }
        Ok(())
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = &app;
        let ch = on_frame;
        state
            .start_receive_link(link_id.clone(), relay, stream, audio)
            .await
            .map_err(|e| e.to_user_string())?;
        let mut frames = match state.take_receive_frames_for(&link_id) {
            Some(r) => r,
            None => return Err("接收链路已启动但没有帧通道".into()),
        };
        tokio::spawn(async move {
            while let Some(f) = frames.recv().await {
                let Some((w, h, data)) =
                    stross_endpoint::rgba_scaled(&f.rgba, f.width, f.height, RECV_MAX_W)
                else {
                    continue;
                };
                let _ = ch.send(pack_frame(w, h, f.pts_ms, &data));
            }
        });
        Ok(())
    }
}

/// 停止指定链路（其它链路不受影响；多端点链接）。
#[tauri::command]
pub fn stop_receive_link(_app: tauri::AppHandle, state: State<'_, Arc<Kernel>>, link_id: String) {
    state.stop_receive_link(&link_id);
    #[cfg(target_os = "android")]
    {
        let _ = &_app;
        let mut guard = ANDROID_PLAYBACK_LINKS.lock().unwrap();
        if let Some(h) = guard.remove(&link_id) {
            h.abort();
        }
    }
}

/// 全部接收链路快照（linkId + 统计；前端面板逐条展示）。
#[tauri::command]
pub fn receive_links(
    state: State<'_, Arc<Kernel>>,
) -> Vec<stross_kernel::receiver::ReceiveLinkView> {
    state.receive_links()
}

#[tauri::command]
pub async fn start_receive(
    app: tauri::AppHandle,
    state: State<'_, Arc<Kernel>>,
    relay: String,
    stream: String,
    audio: stross_endpoint::playback::AudioOut,
    on_frame: Channel<Vec<u8>>,
) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        let _ = &on_frame; // Android 播放不经显示通道（Kotlin MediaCodec 自渲染）
        // 1) 停旧接收：关闭旧编码帧通道 → 旧播放链循环结束并调用 stopPlayback。
        state.stop_receive();
        // 2) 等旧播放链收尾（stopPlayback 完成）再启新链——消灭同插件竞态。
        //    先取出句柄再 await：否则 `if let` 内临时 MutexGuard 跨 await 持有
        //    （非 Send）→ 命令 future 不满足 tauri 的 Send 约束，编译失败。
        let prev_handle = ANDROID_PLAYBACK_LINKS.lock().unwrap().remove("main");
        if let Some(prev) = prev_handle {
            let _ = prev.abort();
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
        ANDROID_PLAYBACK_LINKS
            .lock()
            .unwrap()
            .insert("main".into(), h);
        Ok(())
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = &app; // Android 分支（Kotlin 播放）才用 app
        let ch = on_frame;
        state
            .start_receive(relay, stream, audio)
            .await
            .map_err(|e| e.to_user_string())?;
        let mut frames = match state.take_receive_frames() {
            Some(r) => r,
            None => return Err("接收会话已启动但没有帧通道".into()),
        };
        // 帧转发：同 `start_receive_link` 的二进制通道路径（旧单流 `main`
        // 槽位复用同一显示管线；`on_frame` 由前端逐链路创建）。
        tokio::spawn(async move {
            while let Some(f) = frames.recv().await {
                let Some((w, h, data)) =
                    stross_endpoint::rgba_scaled(&f.rgba, f.width, f.height, RECV_MAX_W)
                else {
                    continue;
                };
                let _ = ch.send(pack_frame(w, h, f.pts_ms, &data));
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
