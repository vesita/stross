//! Android 播放桥（1f-3）：编码帧 → Kotlin MediaCodec 薄壳 → Rust 转换缩放 →
//! 前端 canvas。
//!
//! 职责划分（解码跟不上接收的根治方向）：
//!
//! * **Rust**（本文件 + [`mobile_jni`]）：Annex-B/SPS 解析（[`stross_media::nal`]）、
//!   csd（SPS/PPS）与尺寸提取、积压跳帧、YUV→RGBA 转换缩放
//!   （[`stross_media::yuv`]）、base64 事件规整、解码统计回写。
//! * **Kotlin**（`PlaybackPlugin.kt`）：只剩 MediaCodec 生命周期与编解码 buffer
//!   搬运（系统 API 薄壳），不再做任何位级解析 / 像素转换。
//!
//! 帧消息格式（Rust → Kotlin，JSON）：
//! `{"d": "<base64>", "k": bool, "c": bool, "p": pts_ms, "csd": "<base64 SPS+PPS>", "w": int, "h": int}`
//! * `csd` 仅在关键帧/配置帧下发（Rust 用 [`stross_media::nal::extract_avc_csd`]
//!   解析），Kotlin 用它 + `w/h` 创建解码器，无需自行解析 SPS。

use std::sync::{Arc, Mutex};

use base64::Engine as _;
use serde_json::Value;
use stross_media::nal;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::plugin::PluginHandle;
use tauri::{AppHandle, Manager, Wry};

use stross_media::capture::{CaptureBackend, CaptureStatus};
use stross_media::pipeline::StreamConfig;
use stross_media::playback::AudioOut;
use stross_proto::frame::{
    CODEC_AAC, CODEC_H264, FLAG_CONFIG, FLAG_KEYFRAME, Frame, TRACK_AUDIO, TRACK_VIDEO,
};
use stross_proto::message::{
    CapabilityDescriptor, CapabilityKind, CodecId, MediaKind, ReliabilityProfile, TransportId,
};
use tokio::sync::mpsc;

#[cfg(target_os = "android")]
use crate::mobile_jni;

/// setup 阶段注册的 Android 采集插件句柄（`MediaPlugin`）。
pub struct CapturePluginHandle(pub PluginHandle<Wry>);

/// setup 阶段注册的 Android 播放插件句柄（`PlaybackPlugin`）。
pub struct PlaybackPluginHandle(pub PluginHandle<Wry>);

/// 注册 Android 采集插件（`MediaPlugin`；在 `lib.rs::run` 中装配）。
///
/// 注意：tauri 的 Android 插件注册按 **Rust 插件名** 索引 Kotlin 插件
/// （`PluginManager.load(name, plugin)` 同名覆盖），因此采集与播放必须用
/// **不同的插件名**，否则后注册的类会覆盖先注册的（命令找不到）。
pub fn init_capture() -> tauri::plugin::TauriPlugin<Wry> {
    tauri::plugin::Builder::new("stross-media")
        .setup(|app, api| {
            #[cfg(target_os = "android")]
            {
                let capture = api.register_android_plugin("dev.stross.sender", "MediaPlugin")?;
                app.manage(CapturePluginHandle(capture));
            }
            Ok(())
        })
        .build()
}

/// 注册 Android 播放插件（`PlaybackPlugin`；1f-3，见 [`spawn_android_playback`]）。
pub fn init_playback() -> tauri::plugin::TauriPlugin<Wry> {
    tauri::plugin::Builder::new("stross-media-playback")
        .setup(|app, api| {
            #[cfg(target_os = "android")]
            {
                let playback =
                    api.register_android_plugin("dev.stross.sender", "PlaybackPlugin")?;
                app.manage(PlaybackPluginHandle(playback));
            }
            Ok(())
        })
        .build()
}

/// Android 采集后端：MediaProjection 屏幕采集 + MediaCodec 编码。
///
/// 生命周期：
/// * `start`：保存帧通道 → 建立 [`Channel`]（Kotlin 帧回传）→ 调用 Kotlin `startCapture`
/// * `stop`：调用 Kotlin `stopCapture` → 关闭帧通道（触发推流端优雅 Bye）
/// * `status`：Kotlin 控制帧（`t=9`）异步回报的真实采集状态
pub struct AndroidCapture {
    /// 插件句柄（由 `MobilePluginHandle` 克隆而来）。
    handle: PluginHandle<Wry>,
    /// 推流帧通道；停止时 `take()` 掉以触发优雅 Bye。
    tx: Arc<Mutex<Option<mpsc::Sender<Frame>>>>,
    /// 采集真实状态（由 Kotlin 控制帧 t=9 回传）。
    status: Arc<Mutex<CaptureStatus>>,
}

impl AndroidCapture {
    /// 从托管状态的插件句柄构造。
    pub fn from_app(app: &AppHandle<Wry>) -> Self {
        let handle = app.state::<CapturePluginHandle>().0.clone();
        Self::new(handle)
    }

    pub fn new(handle: PluginHandle<Wry>) -> Self {
        Self {
            handle,
            tx: Arc::new(Mutex::new(None)),
            status: Arc::new(Mutex::new(CaptureStatus::default())),
        }
    }
}

impl CaptureBackend for AndroidCapture {
    fn descriptor(&self) -> CapabilityDescriptor {
        CapabilityDescriptor {
            kind: CapabilityKind::Source,
            media: vec![MediaKind::Screen, MediaKind::Mic],
            codecs: vec![CodecId::H264, CodecId::Aac],
            transports: vec![TransportId::Ws],
            max_width: Some(1920),
            max_height: Some(1080),
            preferred_profile: ReliabilityProfile::Lossy,
        }
    }

    fn start(&self, cfg: &StreamConfig, tx: mpsc::Sender<Frame>) -> anyhow::Result<()> {
        *self.tx.lock().unwrap() = Some(tx);
        *self.status.lock().unwrap() = CaptureStatus::default();

        // 帧通道：Kotlin base64 帧 → 协议帧 → 推流；t=9 为采集状态控制帧
        let tx_chan = self.tx.clone();
        let status = self.status.clone();
        let channel: Channel<Value> = Channel::new(move |body| {
            let v: Value = match body {
                InvokeResponseBody::Json(s) => match serde_json::from_str(&s) {
                    Ok(v) => v,
                    Err(_) => return Ok(()),
                },
                InvokeResponseBody::Raw(_) => return Ok(()),
            };
            let Some(track) = v.get("t").and_then(|x| x.as_u64()) else {
                return Ok(());
            };
            // 采集状态控制帧（不推给中继，只更新状态供前端轮询）
            if track == 9 {
                let mut st = status.lock().unwrap();
                if let Some(started) = v.get("started").and_then(|x| x.as_bool()) {
                    st.started = started;
                    st.error = v.get("err").and_then(|x| x.as_str()).map(|s| s.to_string());
                }
                if v.get("stopped").and_then(|x| x.as_bool()).unwrap_or(false) {
                    st.started = false;
                    st.error = None;
                }
                return Ok(());
            }
            let keyframe = v.get("k").and_then(|x| x.as_bool()).unwrap_or(false);
            let is_config = v.get("c").and_then(|x| x.as_bool()).unwrap_or(false);
            let pts = v.get("p").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
            let Some(data) = v.get("d").and_then(|x| x.as_str()) else {
                return Ok(());
            };
            let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) else {
                return Ok(());
            };
            let (track_id, codec) = if track == 0 {
                (TRACK_VIDEO, CODEC_H264)
            } else {
                (TRACK_AUDIO, CODEC_AAC)
            };
            let mut flags = 0u8;
            if keyframe {
                flags |= FLAG_KEYFRAME;
            }
            if is_config {
                flags |= FLAG_CONFIG;
            }
            let frame = Frame::new(track_id, codec, flags, pts, bytes);
            if let Some(tx) = tx_chan.lock().unwrap().as_ref() {
                // 实时流：通道满则丢弃旧帧
                let _ = tx.try_send(frame);
            }
            Ok(())
        });

        // 调用 Kotlin 插件。注意：run_mobile_plugin 内部**同步阻塞**等待 Kotlin
        // resolve（屏幕录制授权弹窗可能耗时 10s+），而 Tauri 默认单线程 async
        // runtime——直接调用会冻结整个 runtime，导致 capture_status 轮询等
        // 其它命令全部卡住（前端表现为"状态卡住/假推流"）。
        // 因此放到 spawn_blocking 独立线程，授权结果完全由 t=9 控制帧回报。
        let handle = self.handle.clone();
        let payload = serde_json::json!({
            "streamId": cfg.stream_id,
            "title": cfg.title,
            "width": cfg.quality.width,
            "height": cfg.quality.height,
            "fps": cfg.quality.fps,
            "bitrateKbps": cfg.quality.bitrate_kbps,
            "withAudio": cfg.audio.is_some(),
            "channel": channel,
        });
        tokio::task::spawn_blocking(move || {
            match handle.run_mobile_plugin::<serde_json::Value>("startCapture", payload) {
                Ok(v) => tracing::info!("startCapture 命令返回: {v}"),
                Err(e) => tracing::error!("startCapture 命令失败: {e}"),
            }
        });
        Ok(())
    }

    fn stop(&self) {
        // 通知 Kotlin 停止采集（快速返回，无需 spawn_blocking）
        let handle = self.handle.clone();
        let _ = handle.run_mobile_plugin::<serde_json::Value>("stopCapture", serde_json::json!({}));
        // 关闭推流通道 → 客户端发 Bye
        self.tx.lock().unwrap().take();
        *self.status.lock().unwrap() = CaptureStatus::default();
    }

    fn status(&self) -> CaptureStatus {
        self.status.lock().unwrap().clone()
    }
}

// ---------------------------------------------------------------------------
// Android 播放桥（1f-3）：编码帧 → Kotlin MediaCodec 薄壳 → Rust 转换缩放 →
// 前端 canvas
// ---------------------------------------------------------------------------

/// 开始接收消费循环前的积压阈值（帧数）：超过即进入"跳帧追实时"——
/// 丢弃非关键帧直到下一个关键帧。接收侧 mpsc(32) 满时丢新帧，会导致
/// 解码端永远在消费旧帧（画面滞后累积）；跳帧在消费端主动放弃中间帧，
/// 解码负载与画面新鲜度取得平衡（与 B5 关键帧重对齐同思路）。
const DROP_BACKLOG: usize = 8;

/// 启动 Android 播放链路：
///
/// 1. 注册 JNI 桥全局 AppHandle（`mobile_jni::init`）——Kotlin 解码线程随后
///    经 JNI 直调 Rust（`nativeSubmitYuvFrame`：YUV→RGBA 缩放 + base64 事件）。
/// 2. 通知 Kotlin `PlaybackPlugin.startPlayback`（建解码器 + AudioTrack）。
/// 3. 消费 `rx` 里的编码帧：视频帧 →（Rust 解析 SPS 尺寸/csd）`feedVideo`、
///    音频帧 → `feedAudio`；积压超过 [`DROP_BACKLOG`] 时跳非关键帧追实时。
///
/// 接收结束（rx 关闭）时通知 Kotlin 释放解码器 / AudioTrack。
pub fn spawn_android_playback(app: &AppHandle<Wry>, rx: mpsc::Receiver<Frame>, audio: AudioOut) {
    #[cfg(target_os = "android")]
    mobile_jni::init(app);
    let handle = app.state::<PlaybackPluginHandle>().0.clone();

    // 启动 Kotlin 播放器（同步等待 resolve；放 blocking 线程避免冻结 runtime）
    {
        let handle = handle.clone();
        tokio::task::spawn_blocking(move || {
            let _ = handle.run_mobile_plugin::<serde_json::Value>(
                "startPlayback",
                serde_json::json!({ "audio": audio == AudioOut::Device }),
            );
        });
    }

    // 编码帧 → Kotlin（放 blocking 线程：run_mobile_plugin 同步阻塞）
    tokio::task::spawn_blocking(move || {
        let mut rx = rx;
        let mut skipping = false; // 积压跳帧状态：跳非关键帧直到关键帧
        while let Some(f) = rx.blocking_recv() {
            let is_config = f.header.flags & FLAG_CONFIG != 0;
            let keyframe = f.header.flags & FLAG_KEYFRAME != 0;

            // 视频轨道积压跳帧：消费端落后太多时主动丢非关键帧（追实时）。
            // 关键帧（重建参考）与配置帧（建解码器必需）绝不跳过。
            if f.header.track == TRACK_VIDEO {
                if rx.len() >= DROP_BACKLOG {
                    skipping = true;
                }
                if skipping && !keyframe && !is_config {
                    continue;
                }
                if keyframe {
                    skipping = false;
                }

                // 关键帧/配置帧由 Rust 解析 SPS：取 csd（SPS+PPS）+ 尺寸，
                // 随帧下发——Kotlin 直接用，不再自行解析（删 ~120 行 Java 位解析）。
                let (mut w, mut h, mut csd) = (0i32, 0i32, None);
                if (is_config || keyframe)
                    && let Some(cfg) = nal::extract_avc_config(&f.payload)
                {
                    w = cfg.width as i32;
                    h = cfg.height as i32;
                    csd = Some(base64::engine::general_purpose::STANDARD.encode(&cfg.csd));
                }
                let payload = serde_json::json!({
                    "d": base64::engine::general_purpose::STANDARD.encode(&f.payload),
                    "k": keyframe,
                    "c": is_config,
                    "p": f.header.pts_ms,
                    "w": w,
                    "h": h,
                    "csd": csd,
                });
                let _ = handle.run_mobile_plugin::<serde_json::Value>("feedVideo", payload);
            } else if f.header.track == TRACK_AUDIO && audio == AudioOut::Device {
                let payload = serde_json::json!({
                    "d": base64::engine::general_purpose::STANDARD.encode(&f.payload),
                    "p": f.header.pts_ms,
                });
                let _ = handle.run_mobile_plugin::<serde_json::Value>("feedAudio", payload);
            }
        }
        // 接收结束：通知 Kotlin 释放解码器 / AudioTrack
        let _ =
            handle.run_mobile_plugin::<serde_json::Value>("stopPlayback", serde_json::json!({}));
        tracing::info!("Android 播放链路结束");
    });
}
