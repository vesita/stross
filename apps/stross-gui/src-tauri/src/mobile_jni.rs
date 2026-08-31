//! Android 播放原生桥（Kotlin ⇄ Rust JNI 直传）—— **仅解码统计回写**。
//!
//! Surface 渲染路径（见 [`crate::mobile::spawn_android_playback`]）下，`MediaCodec`
//! 解码器**直接输出到 `SurfaceView` 的 Surface**（GPU 直出，零像素搬运），不再有
//! YUV→RGBA 缩放 → 二进制通道 → 前端 canvas 的像素回传链。因此本模块只保留
//! 一个 JNI 入口：Kotlin `PlaybackPlugin` 每渲染一帧回调 `nativeDecodedFrame()`，
//! Rust 侧把解码统计回写到当前播放链路的 `ReceiveStats`（`decoded_video` 计数）。
//!
//! 活动链路（`active_link`）由 `spawn_android_playback` 启动/收尾时
//! [`set_active_link`] / [`clear_active_link`] 维护，JNI 帧回调据此选择正确的
//! 接收链路（多端点链接），回落 `main` 槽。

use std::sync::{OnceLock, RwLock};

use jni::JNIEnv;
use jni::objects::JObject;
use tauri::Emitter;
use tauri::Manager;

/// `spawn_android_playback` 启动时注入的 AppHandle（解码统计回写用）。
static APP: OnceLock<tauri::AppHandle> = OnceLock::new();

/// 当前活动播放链路 id（多端点链接路由；空 = 回落 `main` 槽）。
static ACTIVE_LINK: RwLock<Option<String>> = RwLock::new(None);

/// 注册全局 AppHandle（播放链路启动时调用一次）。
pub fn init(app: &tauri::AppHandle) {
    let _ = APP.set(app.clone());
}

/// 记录当前活动播放链路（多端点链接路由）。
pub fn set_active_link(link_id: &str) {
    *ACTIVE_LINK.write().unwrap() = Some(link_id.to_string());
}

/// 清空活动播放链路（播放链路收尾）。
pub fn clear_active_link() {
    *ACTIVE_LINK.write().unwrap() = None;
}

/// Kotlin 播放线程直调：Kotlin `PlaybackPlugin`（MediaCodec）每渲染一帧回调一次，
/// 把解码统计写回当前活动链路（`decoded_video` 计数）。本函数只做一次锁访问 +
/// 计数自增，**绝不阻塞解码线程**。
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_stross_sender_PlaybackPlugin_nativeDecodedFrame<'local>(
    _env: JNIEnv<'local>,
    _this: JObject<'local>,
) {
    let link = ACTIVE_LINK.read().unwrap().clone();
    let Some(app) = APP.get() else {
        return;
    };
    if let Some(sta) = app.try_state::<std::sync::Arc<stross_kernel::Kernel>>() {
        sta.note_android_decoded_frame_on(link.as_deref().unwrap_or(""));
    }
}

/// Kotlin `PlaybackPlugin`（返回键退出原生全屏）直调：通知前端恢复全屏态
/// （fsActive=false、重定位 surface）。本函数只发一个 Tauri 事件，不阻塞。
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_stross_sender_PlaybackPlugin_nativeFullscreenExited<'local>(
    _env: JNIEnv<'local>,
    _this: JObject<'local>,
) {
    if let Some(app) = APP.get() {
        let _ = app.emit(
            "native-fullscreen-changed",
            serde_json::json!({ "active": false }),
        );
    }
}
