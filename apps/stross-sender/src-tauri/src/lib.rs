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

#[tauri::command]
fn open_viewer(state: State<'_, StrossApp>) -> Result<(), String> {
    if cfg!(target_os = "android") {
        // Android 上 open crate 无法唤起系统浏览器（会报"没有文件或目录"），
        // 请使用「观看」页的内嵌播放器
        return Err("Android 请直接使用「观看」页".into());
    }
    let port = state.stream_relay_port();
    let url = format!("http://127.0.0.1:{port}/");
    open::that(&url).map_err(|e| format!("打开浏览器失败: {e}"))
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
fn create_session(
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
    state.kernel().create_session(&src, &sinks, &prefs)
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
fn teardown_session(state: State<'_, StrossApp>, session_id: String) -> Result<(), String> {
    state.kernel().teardown(&session_id)
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
        open_viewer,
        kernel_nodes,
        kernel_sessions,
        create_session,
        authorize_session,
        route_session,
        teardown_session
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
    let builder = builder.plugin(mobile::init());
    builder
        .run(tauri::generate_context!())
        .expect("Stross 启动失败");
}

// ---------------------------------------------------------------------------
// 无界面中继模式（`stross-sender --relay-only [--port N] [--no-advertise]`）
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
                eprintln!("中继启动失败: {e}");
                return;
            }
        };
        let ips = local_ips();
        println!("\n  📡 Stross 中继（无界面模式）已启动\n");
        for ip in &ips {
            println!("     观看地址: http://{ip}:{}/", handle.port);
        }
        if ips.is_empty() {
            println!("     观看地址: http://127.0.0.1:{}/", handle.port);
        }
        println!("\n     推流地址: ws://<中继IP>:{}/ws/push", handle.port);
        println!("     Ctrl+C 退出\n");

        let _discovery = if advertise {
            match local_ips().into_iter().next() {
                Some(ip) => {
                    match Discovery::start(
                        &format!("sender-relay-{}", handle.port),
                        ip,
                        handle.port,
                        &[("kind", "relay")],
                    ) {
                        Ok(d) => {
                            println!("  mDNS 广播中…");
                            Some(d)
                        }
                        Err(e) => {
                            tracing::warn!("mDNS 广播失败: {e}");
                            None
                        }
                    }
                }
                None => None,
            }
        } else {
            None
        };

        tokio::signal::ctrl_c().await.ok();
        println!("正在停止…");
        handle.stop().await;
        drop(_discovery);
    });
}
