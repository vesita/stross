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

// 桌面接收路径 emit base64 帧载荷需要（Android 走 mobile_jni，不经此模块）。
#[cfg(not(target_os = "android"))]
use base64::Engine as _;
use stross_app::{CaptureStatusView, Platform, StrossApp};
#[cfg(not(mobile))]
use stross_media::capture::FfmpegBackend;
use stross_media::pipeline::StreamConfig;
use tauri::{Emitter, Manager, State};

#[cfg(mobile)]
mod mobile;
// Android 播放 JNI 桥（Kotlin ⇄ Rust 直传）：仅 Android 目标编译，依赖 jni。
#[cfg(all(mobile, target_os = "android"))]
mod mobile_jni;
// 防火墙自动放行（权限自动化）：仅 Linux 桌面（ufw + polkit）。
#[cfg(all(not(mobile), target_os = "linux"))]
mod firewall;
// ---------------------------------------------------------------------------
// 命令面（桌面与 Android 完全一致）
// ---------------------------------------------------------------------------

#[tauri::command]
fn app_info(state: State<'_, Arc<StrossApp>>) -> stross_app::app::AppInfo {
    state.app_info()
}

#[tauri::command]
fn list_devices(state: State<'_, Arc<StrossApp>>) -> stross_app::app::DeviceList {
    state.list_devices()
}

#[tauri::command]
async fn start_relay(
    state: State<'_, Arc<StrossApp>>,
) -> Result<stross_app::app::RelayInfo, String> {
    // 固定端口（含 SRT/QUIC）：防火墙只放行已知端口（权限自动化）
    state
        .start_relay_fixed(
            stross_core::relay::DEFAULT_PORT,
            stross_app::DEFAULT_SRT_PORT,
            stross_app::DEFAULT_QUIC_PORT,
        )
        .await
        .map_err(|e| e.to_user_string())
}

#[tauri::command]
async fn scan_relays(
    state: State<'_, Arc<StrossApp>>,
) -> Result<Vec<stross_app::app::RelayInfo>, String> {
    state.scan_relays().await.map_err(|e| e.to_user_string())
}

#[tauri::command]
async fn start_stream(
    state: State<'_, Arc<StrossApp>>,
    cfg: StreamConfig,
    relay_url: Option<String>,
) -> Result<stross_app::app::StartResult, String> {
    state
        .start_stream(cfg, relay_url)
        .await
        .map_err(|e| e.to_user_string())
}

#[tauri::command]
async fn stop_stream(state: State<'_, Arc<StrossApp>>) -> Result<(), String> {
    state.stop_stream().await.map_err(|e| e.to_user_string())
}

#[tauri::command]
fn stream_status(state: State<'_, Arc<StrossApp>>) -> stross_app::app::StreamStatus {
    state.stream_status()
}

#[tauri::command]
fn capture_status(state: State<'_, Arc<StrossApp>>) -> CaptureStatusView {
    state.capture_status()
}

// ---------------------------------------------------------------------------
// 内核命令（控制面：设备图 / 会话 / 路由，设计文档 §3）
// ---------------------------------------------------------------------------

/// 设备图快照（本机能力 + 发现结果）。
#[tauri::command]
fn kernel_nodes(state: State<'_, Arc<StrossApp>>) -> Vec<stross_app::kernel::NodeInfo> {
    state.kernel().nodes()
}

/// 会话列表快照。
#[tauri::command]
fn kernel_sessions(state: State<'_, Arc<StrossApp>>) -> Vec<stross_app::kernel::Session> {
    state.kernel().sessions()
}

/// 创建会话（「从 `src` 推送到 `sinks`」）。
///
/// `access_code` 可选：设置后该会话启用访问码（PIN），控制操作
/// （route / teardown）需先 `authorize_session`（设计文档 §7）。
#[tauri::command]
fn create_session(
    state: State<'_, Arc<StrossApp>>,
    src: String,
    sinks: Vec<String>,
    access_code: Option<String>,
) -> Result<stross_app::kernel::Session, String> {
    let prefs = stross_app::SessionPrefs {
        profile: stross_proto::message::ReliabilityProfile::Lossy,
        preferred_transport: None,
        access_code,
        title: String::new(),
    };
    state
        .kernel()
        .create_session(&src, &sinks, &prefs)
        .map_err(|e| e.to_user_string())
}

/// 签发「接收手机麦克风」接入凭证（B2）：建会话 + 签发一次性 ShareToken。
///
/// 电脑端点击「接收手机麦克风」时调用：内核建本机会话并签发凭证，返回
/// 凭证字符串（含 stream_id / PIN / 时效）供前端展示，手机出示即可推入
/// 本机受控中继（B0 凭证式接入：零远程控制面暴露）。
#[tauri::command]
fn issue_share_token(
    state: State<'_, Arc<StrossApp>>,
    ttl_secs: Option<u64>,
) -> Result<stross_app::app::ShareTokenView, String> {
    state
        .issue_share_token(ttl_secs)
        .map_err(|e| e.to_user_string())
}

/// 会话鉴权：校验访问码（PIN）；成功后该会话的控制操作放行。
#[tauri::command]
fn authorize_session(
    state: State<'_, Arc<StrossApp>>,
    session_id: String,
    access_code: Option<String>,
) -> Result<(), String> {
    state
        .kernel()
        .authorize(&session_id, access_code.as_deref())
        .map_err(|e| e.to_user_string())
}

/// 控制传输方向（会话存续期间动态改道）。
#[tauri::command]
fn route_session(
    state: State<'_, Arc<StrossApp>>,
    session_id: String,
    path: stross_proto::message::RoutePath,
) -> Result<(), String> {
    state
        .kernel()
        .route(&session_id, path)
        .map_err(|e| e.to_user_string())
}

/// 拆除会话。
#[tauri::command]
fn teardown_session(state: State<'_, Arc<StrossApp>>, session_id: String) -> Result<(), String> {
    state
        .kernel()
        .teardown(&session_id)
        .map_err(|e| e.to_user_string())
}

// ---------------------------------------------------------------------------
// 防火墙自动放行（权限自动化：仅 Linux 桌面，ufw + polkit）
// ---------------------------------------------------------------------------

#[cfg(all(not(mobile), target_os = "linux"))]
fn required_firewall_ports(state: &StrossApp) -> (Vec<String>, Vec<String>) {
    // 实际端口（回退默认）：中继 WS + 凭证协商 TCP；SRT/QUIC UDP
    let (ws, srt, quic) = state.relay_ports().unwrap_or((
        stross_core::relay::DEFAULT_PORT,
        Some(stross_app::DEFAULT_SRT_PORT),
        Some(stross_app::DEFAULT_QUIC_PORT),
    ));
    let tcp = vec![
        format!("{ws}/tcp"),
        format!("{}/tcp", stross_app::DEFAULT_NEGOTIATOR_PORT),
    ];
    let mut udp = Vec::new();
    if let Some(p) = srt {
        udp.push(format!("{p}/udp"));
    }
    if let Some(p) = quic {
        udp.push(format!("{p}/udp"));
    }
    (tcp, udp)
}

/// 防火墙自检：只读执行 `ufw status verbose`，返回缺失放行端口。
///
/// ufw 未启用 / 入站默认允许 → 无需任何规则（跨设备共享不受阻）。
#[cfg(all(not(mobile), target_os = "linux"))]
#[tauri::command]
async fn firewall_status(
    state: State<'_, Arc<StrossApp>>,
) -> Result<firewall::FirewallStatus, String> {
    let out = tokio::process::Command::new("ufw")
        .args(["status", "verbose"])
        .output()
        .await
        .map_err(|e| format!("无法执行 ufw: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "ufw status 失败: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let ips = stross_core::net::local_ips();
    let subnet = firewall::lan_subnet(&ips);
    let mut status = firewall::parse_ufw_verbose(&text);
    let (tcp, udp) = required_firewall_ports(&state);
    let required: Vec<&str> = tcp.iter().chain(udp.iter()).map(|s| s.as_str()).collect();
    status.missing = match subnet {
        Some(sub) => firewall::missing_rules(
            &required,
            &status.rules,
            &sub,
            status.ufw_active,
            status.default_deny_incoming,
        ),
        None => {
            // 无局域网 IPv4（纯回环 / 未联网）：不提示放行
            Vec::new()
        }
    };
    Ok(status)
}

/// 一键放行：经 polkit（`pkexec`）弹一次系统授权，把缺失的 Stross 端口
/// 按本机局域网子网加入 ufw（精确收窄，不放行整个网段）。
#[cfg(all(not(mobile), target_os = "linux"))]
#[tauri::command]
async fn firewall_allow(state: State<'_, Arc<StrossApp>>) -> Result<(), String> {
    let ips = stross_core::net::local_ips();
    let subnet = firewall::lan_subnet(&ips)
        .ok_or_else(|| "未找到局域网 IPv4 地址，无法生成放行规则（请先连接网络）".to_string())?;
    // 只放行当前确实缺失的端口
    let status = match firewall_status(state.clone()).await {
        Ok(s) => s,
        Err(e) => return Err(e),
    };
    if status.missing.is_empty() {
        return Ok(()); // 已就绪
    }
    let (_, _) = required_firewall_ports(&state);
    // missing 形如 "18777/tcp" / "33462/udp"，按协议分组
    let tcp: Vec<String> = status
        .missing
        .iter()
        .filter(|m| m.ends_with("/tcp"))
        .map(|m| m.trim_end_matches("/tcp").to_string())
        .collect();
    let udp: Vec<String> = status
        .missing
        .iter()
        .filter(|m| m.ends_with("/udp"))
        .map(|m| m.trim_end_matches("/udp").to_string())
        .collect();

    run_pkexec_ufw(&subnet, &tcp, "tcp").await?;
    run_pkexec_ufw(&subnet, &udp, "udp").await?;
    tracing::info!("防火墙放行完成: {subnet} tcp={tcp:?} udp={udp:?}");
    Ok(())
}

/// 经 `pkexec` 执行 `ufw allow from <subnet> to any port <ports> proto <proto>`。
#[cfg(all(not(mobile), target_os = "linux"))]
async fn run_pkexec_ufw(subnet: &str, ports: &[String], proto: &str) -> Result<(), String> {
    if ports.is_empty() {
        return Ok(());
    }
    // 一条规则可带多个端口（ufw 支持逗号分隔）
    let port_list = ports.join(",");
    let out = tokio::process::Command::new("pkexec")
        .args([
            "ufw", "allow", "from", subnet, "to", "any", "port", &port_list, "proto", proto,
        ])
        .output()
        .await
        .map_err(|e| format!("无法执行 pkexec/ufw: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "防火墙放行被拒绝或失败（{proto}: {port_list}）: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 凭证自动协商（权限自动化）：/api/negotiator/request + 信任记忆
// ---------------------------------------------------------------------------

/// 协商服务运行句柄（桌面启动后持有；Android 为空——仅作协商客户端）。
struct NegotiatorHandle(Arc<std::sync::Mutex<Option<Arc<stross_app::ShareNegotiator>>>>);

/// 协商事件 → Tauri 事件桥（电脑端 GUI 弹授权确认）。
#[cfg(not(mobile))]
struct NegotiatorUiBridge {
    app: tauri::AppHandle,
}

#[cfg(not(mobile))]
impl stross_app::NegotiatorUi for NegotiatorUiBridge {
    fn request_pending(&self, req: &stross_app::PendingRequest) {
        let _ = self.app.emit("negotiator-request", req);
    }
}

/// 本机持久化身份（device_id / device_name；首次运行生成，之后稳定）。
///
/// 桌面与 Android 共用：设备作为**协商申请方**时向目标设备出示本身份，
/// 目标设备据此做首次人工确认 / 信任记忆。
#[tauri::command]
fn device_identity(app: tauri::AppHandle) -> stross_app::DeviceIdentity {
    let base = app
        .path()
        .app_data_dir()
        .unwrap_or_else(|_| std::env::temp_dir());
    let name = hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|_| "Stross 设备".into());
    stross_app::load_or_create_identity(&base, &name)
}

/// 应答凭证协商请求（电脑端授权确认弹窗操作后调用）。
///
/// 允许时返回签发的凭证（前端据此启动自动接收监听——与「接收手机麦克风」
/// 一致：轮询本机 /api/streams 出现该流即原生接收）。
#[tauri::command]
fn negotiator_respond(
    state: State<'_, NegotiatorHandle>,
    req_id: String,
    allow: bool,
    remember: bool,
) -> Result<Option<stross_app::ShareGrant>, String> {
    match state
        .0
        .lock()
        .map_err(|_| "协商状态锁不可用".to_string())?
        .as_ref()
    {
        Some(neg) => neg.respond(&req_id, allow, remember),
        None => Err("凭证协商服务未启动".into()),
    }
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
    state: State<'_, Arc<StrossApp>>,
    relay: String,
    stream: String,
    audio: stross_media::playback::AudioOut,
) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        state
            .start_receive_raw(relay.clone(), stream.clone())
            .await
            .map_err(|e| e.to_user_string())?;
        let frames = match state.take_receive_raw_frames() {
            Some(r) => r,
            None => return Err("接收会话已启动但没有编码帧通道".into()),
        };
        crate::mobile::spawn_android_playback(&app, frames, audio);
        Ok(())
    }
    #[cfg(not(target_os = "android"))]
    {
        state
            .start_receive(relay, stream, audio)
            .await
            .map_err(|e| e.to_user_string())?;
        let mut frames = match state.take_receive_frames() {
            Some(r) => r,
            None => return Err("接收会话已启动但没有帧通道".into()),
        };
        // 帧转发：RGBA 最近邻缩放到宽度 ≤ 480 → 事件（显示可跳帧，不反压）。
        // 载荷统一为 base64 字符串（桌面/Android 同格式）：serde 直序列化
        // Vec<u8> 会输出每字节一个数字的 JSON 数组（480×270×4 ≈ 51.8 万元素，
        // ~2.5MB/帧），base64 字符串 ~4 倍紧凑且前端 atob 原生解码。
        tokio::spawn(async move {
            while let Some(f) = frames.recv().await {
                let (w, h, data) = scale_rgba(&f.rgba, f.width, f.height, 480);
                let data = base64::engine::general_purpose::STANDARD.encode(data);
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
fn stop_receive(state: State<'_, Arc<StrossApp>>) {
    state.stop_receive();
}

/// 接收统计（帧数 / 解码 / 音频块）。
#[tauri::command]
fn receive_status(state: State<'_, Arc<StrossApp>>) -> stross_app::ReceiveStats {
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
    // generate_handler! 支持在命令上携带属性（展开到 match arm），
    // 防火墙命令仅 Linux 桌面编译注册
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
        issue_share_token,
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
        .manage(Arc::new(app_state))
        .setup(|app| {
            #[cfg(mobile)]
            {
                let backend = Arc::new(mobile::AndroidCapture::from_app(app.handle()));
                app.state::<Arc<StrossApp>>().set_backend(backend);
            }
            // 注入本机持久化身份：mDNS 实例名携带 device_id 前缀，
            // 多设备同端口广播时实例名唯一（否则 mdns-sd 同名互覆盖）。
            // 与 `device_identity` 命令同源（同一 identity.json）。
            {
                let base = app
                    .path()
                    .app_data_dir()
                    .unwrap_or_else(|_| std::env::temp_dir());
                let name = hostname::get()
                    .map(|h| h.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "Stross 设备".into());
                let id = stross_app::load_or_create_identity(&base, &name);
                app.state::<Arc<StrossApp>>().set_identity(id);
            }
            // 凭证协商服务（权限自动化）：桌面启动；Android 仅作客户端不启动
            #[cfg(not(mobile))]
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
                    let app_state = app_handle.state::<Arc<StrossApp>>().inner().clone();
                    // 引导层（docs/endpoint-model.md §0）：目录（L2）与订阅握手端点
                    // （锚定由前端触发；Android 不起协商端点、仅作客户端）
                    match stross_app::bootstrap::start_handshake(app_state, Arc::new(ui), &base)
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
            #[cfg(mobile)]
            {
                app.manage(NegotiatorHandle(Arc::new(std::sync::Mutex::new(None))));
            }
            // 内核事件桥：订阅 KernelEvent，转发为 Tauri 事件「kernel-event」
            // （前端可订阅替代轮询；设计文档 §3.2）
            {
                let mut rx = app.state::<Arc<StrossApp>>().kernel().subscribe();
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
        if let Err(e) = RelayServer::run_standalone(port, advertise, "sender-relay").await {
            tracing::error!("中继启动失败: {e}");
        }
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
