//! Stross 推流端（Tauri 桌面 + Android）—— UI 模块。
//!
//! 交互模型：**先连接中继（本机或局域网内任意一台），再选择推流（发）或观看（收）**。
//!
//! 本层只做两件事：
//!
//! 1. 把 [`stross_kernel::Kernel`]（核心封装模块）注入 Tauri 托管状态
//! 2. 把前端 `invoke` 的每个命令转发给 `Kernel`（薄命令层，见 [`commands`]）
//!
//! 平台差异（ffmpeg 桌面采集 vs Android 原生采集）被隔离在采集后端里：
//! 桌面用 [`stross_endpoint::capture::FfmpegBackend`]，Android 用 `mobile::AndroidCapture`。
//!
//! 模块划分（docs/layering-architecture.md：命令层只做参数转译 + 展示粘合）：
//! * [`commands`]：命令面（含桥接命令：扫描 / 目录 / 订阅 / 凭证申请）
//! * [`receive`]：接收播放域命令（帧缩放 / base64 / 事件转发）
//! * [`firewall`]：防火墙自动放行（仅 Linux 桌面）
//! * [`mobile`]：Android 平台适配（采集 / 播放 / JNI）

use std::sync::Arc;

#[cfg(not(mobile))]
use stross_endpoint::capture::FfmpegBackend;
use stross_kernel::Kernel;
use tauri::{Emitter, Manager};

#[cfg(mobile)]
mod mobile;
// Android 播放 JNI 桥（Kotlin ⇄ Rust 直传）：仅 Android 目标编译，依赖 jni。
#[cfg(all(mobile, target_os = "android"))]
mod mobile_jni;
// 防火墙自动放行（权限自动化）：仅 Linux 桌面（ufw + polkit）。
#[cfg(all(not(mobile), target_os = "linux"))]
mod firewall;

mod commands;
mod receive;

use crate::commands::*;
use crate::receive::*;

/// 协商服务运行句柄（桌面启动后持有；Android 为空——仅作协商客户端）。
pub(crate) struct NegotiatorHandle(
    pub(crate) Arc<std::sync::Mutex<Option<Arc<stross_kernel::ShareNegotiator>>>>,
);

/// 协商事件 → Tauri 事件桥（GUI 弹授权确认；Android 前端同样订阅）。
struct NegotiatorUiBridge {
    app: tauri::AppHandle,
}

impl stross_kernel::NegotiatorUi for NegotiatorUiBridge {
    fn request_pending(&self, req: &stross_kernel::PendingRequest) {
        let _ = self.app.emit("negotiator-request", req);
    }
}

fn invoke_handler() -> impl Fn(tauri::ipc::Invoke<tauri::Wry>) -> bool + Send + Sync + 'static {
    // generate_handler! 支持在命令上携带属性（展开到 match arm），
    // 防火墙命令仅 Linux 桌面编译注册
    tauri::generate_handler![
        app_info,
        discoverable_status,
        set_discoverable,
        list_devices,
        start_relay,
        scan_relays,
        scan_devices,
        probe_relay,
        endpoint_ls,
        endpoint_subscribe,
        endpoint_publish,
        endpoint_unpublish,
        endpoint_stop_share,
        endpoint_subscribe_media,
        local_catalog,
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
        receive_status,
        device_identity,
        negotiator_respond,
        #[cfg(all(not(mobile), target_os = "linux"))]
        firewall_status,
        #[cfg(all(not(mobile), target_os = "linux"))]
        firewall_allow
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

    // 平台判定唯一来源在桥接层（`cfg(target_os)` 只允许出现在那里）
    let app_state = Kernel::new(stross_bridge::devices::platform());
    // 平台设备清单（桥接层单一来源：桌面 = 屏幕/麦克风/系统声音；Android = 麦克风/系统声音）
    stross_bridge::seed_platform_endpoints(&app_state);
    // 桌面后端无依赖，可立即注入；Android 后端需要 plugin setup 阶段
    // 注册的 PluginHandle，只能在 Builder::setup（plugin setup 之后）注入。
    #[cfg(not(mobile))]
    app_state.set_backend(Arc::new(FfmpegBackend::new()));

    let builder = tauri::Builder::default()
        .manage(Arc::new(app_state))
        .setup(|app| {
            #[cfg(mobile)]
            {
                let backend = Arc::new(mobile::AndroidCapture::from_app(app.handle()));
                app.state::<Arc<Kernel>>().set_backend(backend);
            }
            // 注入本机持久化身份：mDNS 实例名携带 device_id 前缀，
            // 多设备同端口广播时实例名唯一（否则 mdns-sd 同名互覆盖）。
            // 与 `device_identity` 命令同源（同一 identity.json）。
            {
                let base = app
                    .path()
                    .app_data_dir()
                    .unwrap_or_else(|_| std::env::temp_dir());
                let name = stross_bridge::hostname_or("Stross 设备");
                let id = stross_kernel::load_or_create_identity(&base, &name);
                app.state::<Arc<Kernel>>().set_identity(id);
                // 「可被发现」（mDNS 广播本机）：读持久化设置并注入内核。
                // 默认关——用户显式开启才被局域网扫描发现。
                let discoverable = stross_kernel::load_settings(&base).discoverable;
                app.state::<Arc<Kernel>>().set_discoverable(discoverable);
            }
            // 凭证协商服务（权限自动化）：所有平台都启动。此前仅桌面启动、
            // Android 只作客户端；为支持「手机通告端点 → 对端订阅」的真机闭环，
            // 解除该限制（Android 也作为公开方被订阅）。
            {
                let handle_arc = Arc::new(std::sync::Mutex::new(None));
                app.manage(NegotiatorHandle(handle_arc.clone()));
                let app_handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    let base = app_handle
                        .path()
                        .app_data_dir()
                        .unwrap_or_else(|_| std::env::temp_dir());
                    let ui = NegotiatorUiBridge {
                        app: app_handle.clone(),
                    };
                    let app_state = app_handle.state::<Arc<Kernel>>().inner().clone();
                    // 引导层（docs/endpoint-model.md §0）：目录（L2）与订阅握手端点
                    match stross_kernel::bootstrap::start_handshake(app_state, Arc::new(ui), &base)
                        .await
                    {
                        Ok(neg) => {
                            tracing::info!("凭证协商端点已启动: 0.0.0.0:{}", neg.port);
                            *handle_arc.lock().unwrap() = Some(neg);
                        }
                        Err(e) => tracing::error!("凭证协商端点启动失败: {e}"),
                    }
                });
            }
            // 内核事件桥：订阅 KernelEvent，转发为 Tauri 事件「kernel-event」
            // （前端可订阅替代轮询；设计文档 §3.2）
            {
                let mut rx = app.state::<Arc<Kernel>>().subscribe();
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
    use stross_kernel::relay::{DEFAULT_PORT, RelayServer};

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
        // mDNS 广播主机名：壳层平台适配负责取本机名（core 零 OS 调用）
        let hostname = stross_bridge::hostname_or("stross");
        if let Err(e) =
            RelayServer::run_standalone(port, advertise, "sender-relay", &hostname).await
        {
            tracing::error!("中继启动失败: {e}");
        }
    });
}
