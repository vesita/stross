//! 移动端（Android）原生采集桥接 —— 系统适配模块的 Android 实现。
//!
//! 实现 [`stross_media::capture::CaptureBackend`]，把 Kotlin 插件（MediaProjection
//! + MediaCodec）包装成统一的采集后端，上层 [`stross_app`] 无需感知平台差异。
//!
//! 分工：
//!
//! * **Rust**（本文件）：实现 [`CaptureBackend`]；用 [`tauri::ipc::Channel`]
//!   接收 Kotlin 插件回传的编码帧（base64 JSON），转成协议帧送入推流通道。
//! * **Kotlin**（`android/MediaPlugin.kt`）：MediaProjection 屏幕采集 + MediaCodec
//!   H.264 编码、AudioRecord + MediaCodec AAC 编码。
//!
//! 帧消息格式（Kotlin → Rust，JSON）：
//! `{"t": 0|1, "k": true|false, "c": true|false, "p": pts_ms, "d": "<base64>"}`
//! * `t` track：0 视频 / 1 音频；`t=9` 为采集状态控制帧
//! * `k` keyframe、`c` config（SPS/PPS 或 AudioSpecificConfig）
//! * `p` 演示时间戳（毫秒）、`d` base64 编码的 Annex-B / ADTS 数据
//!
//! 注意：tauri 2.11 没有公开的 `plugin_handle()` 访问器，`PluginHandle` 只能在
//! setup 阶段由 `register_android_plugin` 取得，因此存入托管状态
//! [`MobilePluginHandle`] 供命令使用。

use std::sync::{Arc, Mutex};

use base64::Engine as _;
use serde_json::Value;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::plugin::PluginHandle;
use tauri::{AppHandle, Manager, Wry};

use stross_media::capture::{CaptureBackend, CaptureStatus};
use stross_media::pipeline::StreamConfig;
use stross_proto::frame::{
    CODEC_AAC, CODEC_H264, FLAG_CONFIG, FLAG_KEYFRAME, Frame, TRACK_AUDIO, TRACK_VIDEO,
};
use stross_proto::message::{CapabilityDescriptor, CapabilityKind, MediaKind, ReliabilityProfile};
use tokio::sync::mpsc;

/// setup 阶段注册的 Android 插件句柄（托管状态，命令通过它调用 Kotlin）。
pub struct MobilePluginHandle(pub PluginHandle<Wry>);

/// 注册 Android 原生插件（在 `lib.rs::run` 中装配）。
pub fn init() -> tauri::plugin::TauriPlugin<Wry> {
    tauri::plugin::Builder::new("stross-media")
        .setup(|app, api| {
            #[cfg(target_os = "android")]
            {
                let handle = api.register_android_plugin("dev.stross.sender", "MediaPlugin")?;
                app.manage(MobilePluginHandle(handle));
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
        let handle = app.state::<MobilePluginHandle>().0.clone();
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
            codecs: vec!["h264".into(), "aac".into()],
            transports: vec!["ws".into()],
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
            let _ = handle.run_mobile_plugin::<serde_json::Value>("startCapture", payload);
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
