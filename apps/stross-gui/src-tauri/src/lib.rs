//! Stross 推流端（Tauri 桌面 + Android）—— UI 模块。
//!
//! 交互模型：**先连接中继（本机或局域网内任意一台），再选择推流（发）或观看（收）**。
//!
//! 本层只做两件事：
//!
//! 1. 把 [`stross_app::StrossApp`]（核心封装模块）注入 Tauri 托管状态
//! 2. 把前端 `invoke` 的每个命令转发给 `StrossApp`（薄命令层）
//!
//! 平台差异（ffmpeg 桌面采集 vs Android 原生采集）被隔离在采集后端里：
//! 桌面用 [`stross_media::capture::FfmpegBackend`]，Android 用 `mobile::AndroidCapture`。

use std::sync::Arc;

use stross_app::{CaptureStatusView, Platform, StrossApp};
#[cfg(not(mobile))]
use stross_media::capture::FfmpegBackend;
use stross_media::pipeline::StreamConfig;
#[cfg(not(mobile))]
use stross_proto::message::{CodecId, DiscoveryInfo, RoleId, TransportId};
use tauri::{Emitter, Manager, State};

#[cfg(mobile)]
mod mobile;
// ---------------------------------------------------------------------------
// 命令面（桌面与 Android 完全一致）
// ---------------------------------------------------------------------------

#[tauri::command]
fn app_info(state: State<'_, StrossApp>) -> stross_app::app::AppInfo {
    state.app_info()
}

#[tauri::command]
fn list_devices(state: State<'_, StrossApp>) -> stross_app::app::DeviceList {
    state.list_devices()
}

#[tauri::command]
async fn start_relay(state: State<'_, StrossApp>) -> Result<stross_app::app::RelayInfo, String> {
    state.start_relay().await
}

#[tauri::command]
async fn scan_relays(
    state: State<'_, StrossApp>,
) -> Result<Vec<stross_app::app::RelayInfo>, String> {
    state.scan_relays().await
}

#[tauri::command]
async fn start_stream(
    state: State<'_, StrossApp>,
    cfg: StreamConfig,
    relay_url: Option<String>,
) -> Result<stross_app::app::StartResult, String> {
    state.start_stream(cfg, relay_url).await
}

#[tauri::command]
async fn stop_stream(state: State<'_, StrossApp>) -> Result<(), String> {
    state.stop_stream().await
}

#[tauri::command]
fn stream_status(state: State<'_, StrossApp>) -> stross_app::app::StreamStatus {
    state.stream_status()
}

#[tauri::command]
fn capture_status(state: State<'_, StrossApp>) -> CaptureStatusView {
    state.capture_status()
}

// ---------------------------------------------------------------------------
// 内核命令（控制面：设备图 / 会话 / 路由，设计文档 §3）
// ---------------------------------------------------------------------------

/// 设备图快照（本机能力 + 发现结果）。
#[tauri::command]
fn kernel_nodes(state: State<'_, StrossApp>) -> Vec<stross_app::kernel::NodeInfo> {
    state.kernel().nodes()
}

/// 会话列表快照。
#[tauri::command]
fn kernel_sessions(state: State<'_, StrossApp>) -> Vec<stross_app::kernel::Session> {
    state.kernel().sessions()
}

/// 创建会话（「从 `src` 推送到 `sinks`」）。
///
/// `access_code` 可选：设置后该会话启用访问码（PIN），控制操作
/// （route / teardown）需先 `authorize_session`（设计文档 §7）。
#[tauri::command]
async fn create_session(
    state: State<'_, StrossApp>,
    src: String,
    sinks: Vec<String>,
    access_code: Option<String>,
) -> Result<stross_app::kernel::Session, String> {
    let prefs = stross_app::SessionPrefs {
        profile: stross_proto::message::ReliabilityProfile::Lossy,
        preferred_transport: None,
        access_code,
    };
    state.kernel().create_session(&src, &sinks, &prefs).await
}

/// 会话鉴权：校验访问码（PIN）；成功后该会话的控制操作放行。
#[tauri::command]
fn authorize_session(
    state: State<'_, StrossApp>,
    session_id: String,
    access_code: Option<String>,
) -> Result<(), String> {
    state
        .kernel()
        .authorize(&session_id, access_code.as_deref())
}

/// 控制传输方向（会话存续期间动态改道）。
#[tauri::command]
fn route_session(
    state: State<'_, StrossApp>,
    session_id: String,
    path: stross_proto::message::RoutePath,
) -> Result<(), String> {
    state.kernel().route(&session_id, path)
}

/// 拆除会话。
#[tauri::command]
async fn teardown_session(state: State<'_, StrossApp>, session_id: String) -> Result<(), String> {
    state.kernel().teardown(&session_id).await
}

// ---------------------------------------------------------------------------
// 接收播放（1e）：WS 收流 → SessionDataManager → PlaybackSink → 前端 canvas
// ---------------------------------------------------------------------------

/// 开始接收 `relay` 上的 `stream`，解码帧缩放后经 `receive-frame` 事件推到前端。
/// `audio` 决定音频去向：`device` 扬声器播放 / `discard` 静音。
///
/// 平台差异（1f-3）：桌面用 ffmpeg 子进程解码（PlaybackSink）；Android 无
/// ffmpeg，走编码帧转发 → Kotlin MediaCodec 解码（`mobile::spawn_android_playback`），
/// 前端事件与绘制完全一致。
#[tauri::command]
async fn start_receive(
    app: tauri::AppHandle,
    state: State<'_, StrossApp>,
    relay: String,
    stream: String,
    audio: stross_media::playback::AudioOut,
) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        state
            .start_receive_raw(relay.clone(), stream.clone())
            .await?;
        let frames = match state.take_receive_raw_frames() {
            Some(r) => r,
            None => return Err("接收会话已启动但没有编码帧通道".into()),
        };
        crate::mobile::spawn_android_playback(&app, frames, audio);
        Ok(())
    }
    #[cfg(not(target_os = "android"))]
    {
        state.start_receive(relay, stream, audio).await?;
        let mut frames = match state.take_receive_frames() {
            Some(r) => r,
            None => return Err("接收会话已启动但没有帧通道".into()),
        };
        // 帧转发：RGBA 最近邻缩放到宽度 ≤ 480 → 事件（显示可跳帧，不反压）
        tokio::spawn(async move {
            while let Some(f) = frames.recv().await {
                let (w, h, data) = scale_rgba(&f.rgba, f.width, f.height, 480);
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
fn stop_receive(state: State<'_, StrossApp>) {
    state.stop_receive();
}

/// 接收统计（帧数 / 解码 / 音频块）。
#[tauri::command]
fn receive_status(state: State<'_, StrossApp>) -> stross_app::ReceiveStats {
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

// ---------------------------------------------------------------------------

fn invoke_handler() -> impl Fn(tauri::ipc::Invoke<tauri::Wry>) -> bool + Send + Sync + 'static {
    tauri::generate_handler![
        app_info,
        list_devices,
        start_relay,
        scan_relays,
        start_stream,
        stop_stream,
        stream_status,
        capture_status,
        kernel_nodes,
        kernel_sessions,
        create_session,
        authorize_session,
        route_session,
        teardown_session,
        start_receive,
        stop_receive,
        receive_status
    ]
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Android 上 Rust tracing 输出到 logcat，桌面输出到 stderr
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let platform = if cfg!(target_os = "android") {
        Platform::Android
    } else {
        Platform::Desktop
    };
    let app_state = StrossApp::new(platform);
    // 桌面后端无依赖，可立即注入；Android 后端需要 plugin setup 阶段
    // 注册的 PluginHandle，只能在 Builder::setup（plugin setup 之后）注入。
    #[cfg(not(mobile))]
    app_state.set_backend(Arc::new(FfmpegBackend::new()));

    let builder = tauri::Builder::default()
        .manage(app_state)
        .setup(|app| {
            #[cfg(mobile)]
            {
                let backend = Arc::new(mobile::AndroidCapture::from_app(app.handle()));
                app.state::<StrossApp>().set_backend(backend);
            }
            // 内核事件桥：订阅 KernelEvent，转发为 Tauri 事件「kernel-event」
            // （前端可订阅替代轮询；设计文档 §3.2）
            {
                let mut rx = app.state::<StrossApp>().kernel().subscribe();
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    while let Ok(ev) = rx.recv().await {
                        let _ = handle.emit("kernel-event", ev);
                    }
                });
            }
            Ok(())
        })
        .invoke_handler(invoke_handler());
    #[cfg(mobile)]
    let builder = builder
        .plugin(mobile::init_capture())
        .plugin(mobile::init_playback());
    builder
        .run(tauri::generate_context!())
        .expect("Stross 启动失败");
}

// ---------------------------------------------------------------------------
// 无界面中继模式（`stross-gui --relay-only [--port N] [--no-advertise]`）
// ---------------------------------------------------------------------------

/// PC 端整合：桌面应用已内嵌中继；此模式让同一二进制在不启动 GUI 的情况下
/// 单独充当局域网中继（服务器 / 常驻部署场景，不依赖 webkit/GTK）。
#[cfg(not(mobile))]
pub fn run_relay_only(args: &[String]) {
    use stross_core::discovery::Discovery;
    use stross_core::net::local_ips;
    use stross_core::relay::{DEFAULT_PORT, RelayServer};

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // 解析参数：--port N（默认 8777）、--no-advertise（关闭 mDNS 广播）
    let mut port = DEFAULT_PORT;
    let mut advertise = true;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                if let Some(v) = args.get(i + 1).and_then(|s| s.parse().ok()) {
                    port = v;
                    i += 1;
                }
            }
            "--no-advertise" => advertise = false,
            _ => {}
        }
        i += 1;
    }

    let rt = tokio::runtime::Runtime::new().expect("创建 tokio runtime 失败");
    rt.block_on(async {
        let handle = match RelayServer::start(port).await {
            Ok(h) => h,
            Err(e) => {
                tracing::error!("中继启动失败: {e}");
                return;
            }
        };
        let ips = local_ips();
        tracing::info!("📡 Stross 中继（无界面模式）已启动");
        if ips.is_empty() {
            tracing::info!("中继入口: http://127.0.0.1:{}/", handle.port);
        }
        for ip in &ips {
            tracing::info!("中继入口: http://{ip}:{}/", handle.port);
        }
        tracing::info!("推流地址: ws://<中继IP>:{}/ws/push", handle.port);
        tracing::info!("Ctrl+C 退出");

        let _discovery = if advertise {
            match Discovery::start(
                &format!("sender-relay-{}", handle.port),
                &local_ips(),
                handle.port,
                &DiscoveryInfo {
                    v: DiscoveryInfo::VERSION,
                    name: "Stross 中继".into(),
                    roles: vec![RoleId::Relay, RoleId::Sender, RoleId::Viewer],
                    media: vec![],
                    transports: vec![
                        TransportId::Ws,
                        TransportId::WebRtc,
                        TransportId::Srt,
                        TransportId::Quic,
                    ],
                    codecs: vec![CodecId::H264, CodecId::Aac],
                },
            ) {
                Ok(d) => {
                    tracing::info!("mDNS 广播中…");
                    Some(d)
                }
                Err(e) => {
                    tracing::warn!("mDNS 广播失败: {e}");
                    None
                }
            }
        } else {
            None
        };

        tokio::signal::ctrl_c().await.ok();
        tracing::info!("正在停止…");
        handle.stop().await;
        drop(_discovery);
    });
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
