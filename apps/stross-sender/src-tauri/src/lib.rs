//! Stross 推流端（Tauri 桌面 + Android）。
//!
//! 交互模型：**先连接中继（本机或局域网内任意一台），再选择推流（发）或观看（收）**。
//!
//! 命令层把 stross-core 的能力暴露给前端：
//!
//! * `app_info`      —— 版本 / ffmpeg 是否可用 / 本机 IP
//! * `list_devices`  —— 摄像头、麦克风、系统声音设备列表
//! * `start_relay`   —— 启动/复用本机中继（连接阶段）
//! * `scan_relays`   —— mDNS 扫描局域网内其它中继
//! * `start_stream`  —— 推流到指定中继（`relay_url`；None 时推到已连接的中继）
//! * `stop_stream`   —— 停止推流
//! * `stream_status` —— 推流状态
//! * `open_viewer`   —— 在系统浏览器打开观看端页面

use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use stross_core::devices::{list_audio_inputs, list_cameras, list_system_audio, CameraDevice};
use stross_core::net::local_ips;
use stross_core::pipeline::{ffmpeg_available, StreamConfig};
use stross_core::relay::{RelayHandle, RelayServer, DEFAULT_PORT};
use stross_core::sender::SenderEngine;
use tauri::State;

#[cfg(mobile)]
mod mobile;

/// 应用全局状态。
pub struct AppState {
    engine: Mutex<Option<RunningStream>>,
    /// 常驻本机中继（连接阶段启动，观看与推流共用）。
    pub relay: Mutex<Option<RelayHandle>>,
    /// mDNS 广播句柄（本机中继启动时广播，便于局域网内设备扫描发现）。
    pub discovery: Mutex<Option<stross_core::discovery::Discovery>>,
    /// Android 原生采集会话（仅移动端）。
    #[cfg(mobile)]
    pub mobile: Mutex<Option<mobile::MobileCapture>>,
    /// Android 采集真实状态（Kotlin 控制帧回传，供前端轮询）。
    #[cfg(mobile)]
    pub mobile_status: std::sync::Arc<std::sync::Mutex<mobile::MobileCaptureStatus>>,
}

/// 运行中的推流。
struct RunningStream {
    engine: SenderEngine,
    relay_port: u16,
    title: String,
    stream_id: String,
    started_at: u64,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            engine: Mutex::new(None),
            relay: Mutex::new(None),
            discovery: Mutex::new(None),
            #[cfg(mobile)]
            mobile: Mutex::new(None),
            #[cfg(mobile)]
            mobile_status: std::sync::Arc::new(std::sync::Mutex::new(
                mobile::MobileCaptureStatus::default(),
            )),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AppInfo {
    version: String,
    /// "desktop" | "android" —— 前端据此选择 ffmpeg 管线还是原生采集
    platform: String,
    ffmpeg: bool,
    ips: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceList {
    cameras: Vec<CameraDevice>,
    audio_inputs: Vec<String>,
    system_audio: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StartResult {
    relay_port: u16,
    watch_urls: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StreamStatus {
    running: bool,
    stream_id: Option<String>,
    title: Option<String>,
    relay_port: Option<u16>,
    started_at: Option<u64>,
}

// ---------------------------------------------------------------------------

#[tauri::command]
fn app_info() -> AppInfo {
    AppInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        platform: cfg!(target_os = "android").then_some("android").unwrap_or("desktop").to_string(),
        ffmpeg: ffmpeg_available(),
        ips: local_ips().into_iter().map(|ip| ip.to_string()).collect(),
    }
}

#[tauri::command]
fn list_devices() -> DeviceList {
    DeviceList {
        cameras: list_cameras(),
        audio_inputs: list_audio_inputs(),
        system_audio: list_system_audio(),
    }
}

/// 中继信息（连接阶段返回）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RelayInfo {
    port: u16,
    urls: Vec<String>,
}

fn relay_info(port: u16) -> RelayInfo {
    let ips = local_ips();
    let mut urls: Vec<String> = ips
        .iter()
        .map(|ip| format!("http://{ip}:{port}/"))
        .collect();
    if urls.is_empty() {
        urls.push(format!("http://127.0.0.1:{port}/"));
    }
    RelayInfo { port, urls }
}

/// 启动/复用本机中继（"先连接"步骤的本机选项）。
#[tauri::command]
async fn start_relay(state: State<'_, AppState>) -> Result<RelayInfo, String> {
    {
        let guard = state.relay.lock().unwrap();
        if let Some(r) = guard.as_ref() {
            return Ok(relay_info(r.port));
        }
    }
    let handle = RelayServer::start(DEFAULT_PORT)
        .await
        .map_err(|e| e.to_string())?;
    let port = handle.port;
    *state.relay.lock().unwrap() = Some(handle);
    // mDNS 广播本机中继，局域网内其它设备（如电脑端 Stross）可扫描发现，
    // 免去手动输入地址
    if let Some(ip) = local_ips().into_iter().next() {
        let instance = format!("sender-{port}");
        match stross_core::discovery::Discovery::start(
            &instance,
            ip,
            port,
            &[("kind", "relay"), ("name", "Stross 本机中继")],
        ) {
            Ok(d) => {
                *state.discovery.lock().unwrap() = Some(d);
            }
            Err(e) => tracing::warn!("mDNS 广播失败: {e}"),
        }
    }
    Ok(relay_info(port))
}

/// mDNS 扫描局域网内的其它中继。
#[tauri::command]
async fn scan_relays() -> Result<Vec<RelayInfo>, String> {
    let found = stross_core::discovery::Discovery::browse(Duration::from_secs(2))
        .await
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for d in found {
        out.push(RelayInfo {
            port: d.port,
            urls: vec![format!("http://{}:{}/", d.ip, d.port)],
        });
    }
    Ok(out)
}

#[tauri::command]
async fn start_stream(
    state: State<'_, AppState>,
    cfg: StreamConfig,
    relay_url: Option<String>,
) -> Result<StartResult, String> {
    {
        let guard = state.engine.lock().unwrap();
        if guard.is_some() {
            return Err("已经在推流中，请先停止".into());
        }
    }
    // 未指定中继时，推到已连接（常驻）的本机中继
    let relay_url = match relay_url {
        Some(u) => Some(u),
        None => {
            let guard = state.relay.lock().unwrap();
            guard
                .as_ref()
                .map(|r| format!("ws://127.0.0.1:{}/ws/push", r.port))
        }
    };
    // 注意：不能在持有 std MutexGuard 时 await（非 Send），
    // 因此先启动引擎，再写入状态。
    let engine = SenderEngine::start(cfg.clone(), relay_url, DEFAULT_PORT)
        .await
        .map_err(|e| e.to_string())?;
    let relay_port = engine.relay_port().unwrap_or(DEFAULT_PORT);
    let ips = local_ips();
    let mut watch_urls: Vec<String> = ips
        .iter()
        .map(|ip| format!("http://{ip}:{relay_port}/"))
        .collect();
    if watch_urls.is_empty() {
        watch_urls.push(format!("http://127.0.0.1:{relay_port}/"));
    }
    let started_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut guard = state.engine.lock().unwrap();
    *guard = Some(RunningStream {
        engine,
        relay_port,
        title: cfg.title.clone(),
        stream_id: cfg.stream_id.clone(),
        started_at,
    });
    Ok(StartResult {
        relay_port,
        watch_urls,
    })
}

#[tauri::command]
async fn stop_stream(state: State<'_, AppState>) -> Result<(), String> {
    let engine = state.engine.lock().unwrap().take();
    if let Some(stream) = engine {
        tokio::spawn(async move {
            stream.engine.stop().await;
        });
    }
    Ok(())
}

#[tauri::command]
fn stream_status(state: State<'_, AppState>) -> StreamStatus {
    let guard = state.engine.lock().unwrap();
    match guard.as_ref() {
        Some(s) => StreamStatus {
            running: true,
            stream_id: Some(s.stream_id.clone()),
            title: Some(s.title.clone()),
            relay_port: Some(s.relay_port),
            started_at: Some(s.started_at),
        },
        None => StreamStatus {
            running: false,
            stream_id: None,
            title: None,
            relay_port: None,
            started_at: None,
        },
    }
}

#[tauri::command]
fn open_viewer(state: State<'_, AppState>) -> Result<(), String> {
    if cfg!(target_os = "android") {
        // Android 上 open crate 无法唤起系统浏览器（会报"没有文件或目录"），
        // 请使用「观看」页的内嵌播放器
        return Err("Android 请直接使用「观看」页".into());
    }
    let guard = state.engine.lock().unwrap();
    let port = guard.as_ref().map(|s| s.relay_port).unwrap_or(DEFAULT_PORT);
    let url = format!("http://127.0.0.1:{port}/");
    open::that(&url).map_err(|e| format!("打开浏览器失败: {e}"))
}

// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------

/// 无界面中继模式（`stross-sender --relay-only [--port N] [--no-advertise]`）。
///
/// PC 端整合：桌面应用已内嵌中继；此模式让同一二进制在不启动 GUI 的情况下
/// 单独充当局域网中继（服务器 / 常驻部署场景，不依赖 webkit/GTK）。
#[cfg(not(mobile))]
pub fn run_relay_only(args: &[String]) {
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
                    match stross_core::discovery::Discovery::start(
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

/// 桌面端命令集。
#[cfg(not(mobile))]
fn invoke_handler() -> impl Fn(tauri::ipc::Invoke<tauri::Wry>) -> bool + Send + Sync + 'static {
    tauri::generate_handler![
        app_info,
        list_devices,
        start_relay,
        scan_relays,
        start_stream,
        stop_stream,
        stream_status,
        open_viewer
    ]
}

/// 移动端命令集（桌面命令 + 原生采集命令）。
#[cfg(mobile)]
fn invoke_handler() -> impl Fn(tauri::ipc::Invoke<tauri::Wry>) -> bool + Send + Sync + 'static {
    tauri::generate_handler![
        app_info,
        list_devices,
        start_relay,
        scan_relays,
        start_stream,
        stop_stream,
        stream_status,
        open_viewer,
        mobile::start_capture,
        mobile::stop_capture,
        mobile::mobile_status
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

    let builder = tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(invoke_handler());
    #[cfg(mobile)]
    let builder = builder.plugin(mobile::init());
    builder
        .run(tauri::generate_context!())
        .expect("Stross 启动失败");
}
