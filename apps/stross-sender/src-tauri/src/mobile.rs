//! 移动端（Android）原生采集桥接。
//!
//! 分工：
//!
//! * **Rust**（本文件）：启动内嵌中继 + WS 推流客户端；用 [`tauri::ipc::Channel`]
//!   接收 Kotlin 插件回传的编码帧（base64 JSON），转成协议帧送入推流通道。
//! * **Kotlin**（`android/MediaPlugin.kt`）：MediaProjection 屏幕采集 + MediaCodec
//!   H.264 编码、AudioRecord + MediaCodec AAC 编码。
//!
//! 帧消息格式（Kotlin → Rust，JSON）：
//! `{"t": 0|1, "k": true|false, "c": true|false, "p": pts_ms, "d": "<base64>"}`
//! * `t` track：0 视频 / 1 音频
//! * `k` keyframe、`c` config（SPS/PPS 或 AudioSpecificConfig）
//! * `p` 演示时间戳（毫秒）、`d` base64 编码的 Annex-B / ADTS 数据
//!
//! 注意：tauri 2.11 没有公开的 `plugin_handle()` 访问器，`PluginHandle` 只能在
//! setup 阶段由 `register_android_plugin` 取得，因此存入托管状态
//! [`MobilePluginHandle`] 供命令使用。

use std::sync::{Arc, Mutex};

use base64::Engine as _;
use serde::Deserialize;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::plugin::PluginHandle;
use tauri::{AppHandle, Manager, Wry};

use stross_core::pipeline::{Quality, StreamConfig, VideoSource};
use stross_core::relay::{RelayHandle, RelayServer};
use stross_core::sender::RelayClient;
use stross_proto::frame::{
    Frame, CODEC_AAC, CODEC_H264, FLAG_CONFIG, FLAG_KEYFRAME, TRACK_AUDIO, TRACK_VIDEO,
};
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

/// Kotlin 侧启动参数（与 `MediaPlugin.startCapture` 的 InvokeArg 对应）。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureArgs {
    pub stream_id: String,
    pub title: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub with_audio: bool,
}

/// 运行中的 Android 采集会话（由 `AppState` 持有）。
pub struct MobileCapture {
    pub client: RelayClient,
    /// 自己创建的中继（复用已连接中继时为 None，交给 AppState 常驻管理）。
    pub relay: Option<RelayHandle>,
    /// 推流帧通道；停止时 `take()` 掉以触发优雅 Bye。
    pub tx: Arc<Mutex<Option<mpsc::Sender<Frame>>>>,
}

/// Android 采集真实状态（由 Kotlin 控制帧 t=9 回传）。
#[derive(Default, Clone)]
pub struct MobileCaptureStatus {
    /// 采集是否真正启动（虚拟显示 + 编码器就绪）。
    pub started: bool,
    /// 启动失败原因。
    pub error: Option<String>,
}

/// 启动 Android 采集：复用已连接中继 + 推流客户端 + Kotlin 插件。
#[tauri::command]
pub async fn start_capture(
    app: AppHandle<Wry>,
    args: CaptureArgs,
) -> Result<serde_json::Value, String> {
    // 1) 复用「连接」阶段启动的本机中继；没有则新建（不持有 std MutexGuard 跨 await）
    let (relay_port, owned_relay) = {
        let state = app.state::<crate::AppState>();
        let existing = state.relay.lock().unwrap().as_ref().map(|r| r.port);
        match existing {
            Some(port) => (port, None),
            None => {
                let handle = RelayServer::start(stross_core::relay::DEFAULT_PORT)
                    .await
                    .map_err(|e| e.to_string())?;
                let port = handle.port;
                *state.relay.lock().unwrap() = Some(handle);
                (port, None) // 已交给 AppState 常驻
            }
        }
    };
    let _ = owned_relay;

    // 2) 推流客户端（Hello 的轨道信息为占位，实际以 Kotlin 首帧为准）
    let cfg = StreamConfig {
        stream_id: args.stream_id.clone(),
        title: args.title.clone(),
        video: Some(VideoSource::Synthetic {
            pattern: "android".into(),
        }),
        quality: Quality {
            width: args.width,
            height: args.height,
            fps: args.fps,
            bitrate_kbps: args.bitrate_kbps,
        },
        audio: if args.with_audio {
            Some(Default::default())
        } else {
            None
        },
        duration_secs: None,
    };
    let url = format!("ws://127.0.0.1:{relay_port}/ws/push");
    let (client, tx) = RelayClient::connect(&url, &cfg)
        .await
        .map_err(|e| e.to_string())?;
    let tx = Arc::new(Mutex::new(Some(tx)));

    // 3) 帧通道：Kotlin base64 帧 → 协议帧 → 推流；t=9 为采集状态控制帧
    let tx_chan = tx.clone();
    let status = app.state::<crate::AppState>().mobile_status.clone();
    let channel: Channel<serde_json::Value> = Channel::new(move |body| {
        let v: serde_json::Value = match body {
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

    // 4) 调用 Kotlin 插件。注意：run_mobile_plugin 内部**同步阻塞**等待 Kotlin
    //    resolve（屏幕录制授权弹窗可能耗时 10s+），而 Tauri 默认单线程 async
    //    runtime——直接调用会冻结整个 runtime，导致 mobile_status 轮询等
    //    其它命令全部卡住（前端表现为"状态卡住/假推流"）。
    //    因此放到 spawn_blocking 独立线程，授权结果完全由 t=9 控制帧回报。
    let handle = app.state::<MobilePluginHandle>().0.clone();
    let payload = serde_json::json!({
        "streamId": args.stream_id,
        "width": args.width,
        "height": args.height,
        "fps": args.fps,
        "bitrateKbps": args.bitrate_kbps,
        "withAudio": args.with_audio,
        "channel": channel,
    });
    tokio::task::spawn_blocking(move || {
        let _ = handle.run_mobile_plugin::<serde_json::Value>("startCapture", payload);
    });

    // 5) 记录会话（复用中继时不持有、不停止；由 AppState 常驻管理）
    //    同时重置采集状态，等待 Kotlin 控制帧回报真实状态
    let state = app.state::<crate::AppState>();
    *state.mobile.lock().unwrap() = Some(MobileCapture {
        client,
        relay: owned_relay,
        tx,
    });
    *state.mobile_status.lock().unwrap() = MobileCaptureStatus::default();
    Ok(serde_json::json!({ "relayPort": relay_port }))
}

/// 查询 Android 采集真实状态（前端轮询用）。
#[tauri::command]
pub async fn mobile_status(app: AppHandle<Wry>) -> Result<serde_json::Value, String> {
    let state = app.state::<crate::AppState>();
    let active = state.mobile.lock().unwrap().is_some();
    let st = state.mobile_status.lock().unwrap().clone();
    Ok(serde_json::json!({
        "active": active,
        "started": st.started,
        "error": st.error,
    }))
}

/// 停止 Android 采集。
#[tauri::command]
pub async fn stop_capture(app: AppHandle<Wry>) -> Result<(), String> {
    let state = app.state::<crate::AppState>();
    let capture = state.mobile.lock().unwrap().take();
    if let Some(cap) = capture {
        // 通知 Kotlin 停止采集
        let handle = app.state::<MobilePluginHandle>().0.clone();
        let _ = handle.run_mobile_plugin::<serde_json::Value>("stopCapture", serde_json::json!({}));
        // 关闭推流通道 → 客户端发 Bye
        cap.tx.lock().unwrap().take();
        cap.client.stop().await;
        if let Some(relay) = cap.relay {
            relay.stop().await;
        }
    }
    Ok(())
}
