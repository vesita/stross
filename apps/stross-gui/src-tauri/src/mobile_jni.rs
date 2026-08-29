//! Android 播放原生桥（Kotlin ⇄ Rust JNI 直传）。
//!
//! 目标：**Java 只留 MediaCodec/AudioTrack 系统 API 薄壳**；YUV→RGBA 转换、
//! 缩放、事件规整（base64）、解码统计回写全部在 Rust 完成——解码跟不上接收
//! 的 CPU 大头（纯 Java 逐像素转换 + 51.8 万元素 JSON 数组事件）随之消除。
//!
//! Kotlin（`PlaybackPlugin.kt`）解码线程拿到 YUV 后调用本模块导出的 JNI
//! 函数（声明为类内 `external fun`，符号 `Java_dev_stross_sender_PlaybackPlugin_*`）：
//!
//! ```kotlin
//! private external fun nativeSubmitYuvFrame(
//!     yuv: ByteArray, w: Int, h: Int,
//!     colorFormat: Int, strideY: Int, sliceH: Int, pts: Long,
//! )
//! ```
//!
//! Rust 侧完成：`stross_endpoint::yuv::yuv420_to_rgba_scaled` 转换缩放 →
//! base64 编码 → `receive-frame` 事件（与桌面接收路径同一前端事件）→
//! 解码统计回写（`Kernel::note_android_decoded_frame`）。

use std::sync::OnceLock;

use base64::Engine as _;
use jni::JNIEnv;
use jni::objects::{JByteArray, JObject};
use jni::sys::{jint, jlong};
use tauri::{Emitter, Manager};

use stross_endpoint::convert::yuv::{Yuv420Layout, yuv420_to_rgba_scaled};

/// `spawn_android_playback` 启动时注入的 AppHandle（JNI 线程 emit 事件用）。
static APP: OnceLock<tauri::AppHandle> = OnceLock::new();

/// 注册全局 AppHandle（播放链路启动时调用一次）。
pub fn init(app: &tauri::AppHandle) {
    let _ = APP.set(app.clone());
}

/// YUV 帧的最大回传宽度：Android 小屏 + WebView IPC 弱，480 足够
/// （桌面 `receive.rs::RECV_MAX_W = 720`，权衡见该处注释）。
const MAX_FRAME_W: u32 = 480;

/// Kotlin 播放线程直调：YUV420 → RGBA（缩放）→ base64 事件 → 解码统计。
///
/// `color_format`：19 = YUV420Planar（I420）、21 = YUV420SemiPlanar（NV12）。
/// 本函数无返回值：`receive-frame` 事件由 Rust 侧直接发出（右值 overload，
/// Kotlin 侧声明为 `Unit`）。
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_stross_sender_PlaybackPlugin_nativeSubmitYuvFrame<'local>(
    env: JNIEnv<'local>,
    _this: JObject<'local>,
    yuv: JByteArray<'local>,
    w: jint,
    h: jint,
    color_format: jint,
    stride_y: jint,
    slice_h: jint,
    pts: jlong,
) {
    let Ok(bytes) = env.convert_byte_array(&yuv) else {
        return;
    };
    let layout = match color_format {
        19 => Yuv420Layout::Planar,
        21 => Yuv420Layout::SemiPlanar,
        _ => return,
    };
    let Some((tw, th, rgba)) = yuv420_to_rgba_scaled(
        &bytes,
        w.max(1) as u32,
        h.max(1) as u32,
        layout,
        stride_y.max(1) as u32,
        slice_h.max(1) as u32,
        MAX_FRAME_W,
    ) else {
        return;
    };
    // 事件规整（base64 字符串，替代 serde 把 Vec<u8> 序列化成 51.8 万元素
    // 数字数组的爆炸载荷）→ 前端 atob 解码 → canvas。
    if let Some(app) = APP.get() {
        let data = base64::engine::general_purpose::STANDARD.encode(&rgba);
        let _ = app.emit(
            "receive-frame",
            serde_json::json!({ "pts": pts, "width": tw, "height": th, "data": data }),
        );
        // 解码统计回写（Android 解码在 Kotlin 侧，此处与桌面解码线程同口径）
        if let Some(sta) = app.try_state::<stross_kernel::Kernel>() {
            sta.note_android_decoded_frame();
        }
    }
}
