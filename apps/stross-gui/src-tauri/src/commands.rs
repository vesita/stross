//! GUI 命令面（Tauri `invoke` 薄命令层）——**桥**：前端 JS 不直接当协议
//! 客户端，一律经这里调 `stross_app` 库接口（docs/layering-architecture.md）。
//!
//! 与原桌面/Android 共用同一命令面；命令只做参数转译 + 错误 -> String，
//! 逻辑全部在库层（`StrossApp` / `subscriber` / `devices` 等）。

use std::sync::Arc;

use stross_app::{CaptureStatusView, StrossApp};
use stross_media::pipeline::StreamConfig;
use tauri::{Manager, State};

use crate::NegotiatorHandle;

// ---------------------------------------------------------------------------
// 本机状态 / 发现 / 推流
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn app_info(state: State<'_, Arc<StrossApp>>) -> stross_app::app::AppInfo {
    state.app_info()
}

#[tauri::command]
pub fn list_devices(state: State<'_, Arc<StrossApp>>) -> stross_app::app::DeviceList {
    state.list_devices()
}

#[tauri::command]
pub async fn start_relay(
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

/// mDNS 扫描（仅发现；探测/聚合走 [`scan_devices`]——前端不再自写 HTTP 探测）。
#[tauri::command]
pub async fn scan_relays(
    state: State<'_, Arc<StrossApp>>,
) -> Result<Vec<stross_app::app::RelayInfo>, String> {
    state.scan_relays().await.map_err(|e| e.to_user_string())
}

#[tauri::command]
pub async fn start_stream(
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
pub async fn stop_stream(state: State<'_, Arc<StrossApp>>) -> Result<(), String> {
    state.stop_stream().await.map_err(|e| e.to_user_string())
}

#[tauri::command]
pub fn stream_status(state: State<'_, Arc<StrossApp>>) -> stross_app::app::StreamStatus {
    state.stream_status()
}

#[tauri::command]
pub fn capture_status(state: State<'_, Arc<StrossApp>>) -> CaptureStatusView {
    state.capture_status()
}

// ---------------------------------------------------------------------------
// 内核命令（控制面：设备图 / 会话 / 路由，设计文档 §3）
// ---------------------------------------------------------------------------

/// 设备图快照（本机能力 + 发现结果）。
#[tauri::command]
pub fn kernel_nodes(state: State<'_, Arc<StrossApp>>) -> Vec<stross_app::kernel::NodeInfo> {
    state.kernel().nodes()
}

/// 会话列表快照。
#[tauri::command]
pub fn kernel_sessions(state: State<'_, Arc<StrossApp>>) -> Vec<stross_app::kernel::Session> {
    state.kernel().sessions()
}

/// 创建会话（「从 `src` 推送到 `sinks`」）。
#[tauri::command]
pub fn create_session(
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
#[tauri::command]
pub fn issue_share_token(
    state: State<'_, Arc<StrossApp>>,
    ttl_secs: Option<u64>,
) -> Result<stross_app::app::ShareTokenView, String> {
    state
        .issue_share_token(ttl_secs)
        .map_err(|e| e.to_user_string())
}

/// 会话鉴权：校验访问码（PIN）；成功后该会话的控制操作放行。
#[tauri::command]
pub fn authorize_session(
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
pub fn route_session(
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
pub fn teardown_session(
    state: State<'_, Arc<StrossApp>>,
    session_id: String,
) -> Result<(), String> {
    state
        .kernel()
        .teardown(&session_id)
        .map_err(|e| e.to_user_string())
}

// ---------------------------------------------------------------------------
// 桥接：局域网扫描 / 目录 / 订阅 / 凭证申请（前端不再自写协议客户端）
// ---------------------------------------------------------------------------

/// 全量扫描局域网设备（mDNS + HTTP 探测聚合；与 CLI `stross devices` 同源
/// `stross_app::devices::scan`）。返回含在线共享 / SRT / QUIC 的完整视图，
/// 前端每轮刷新只需调它，不再自行 fetch `/api/info` `/api/streams`。
///
/// `extra_base_urls`：手动添加的地址（无 mDNS），一并探测并入结果。
#[tauri::command]
pub async fn scan_devices(
    probe_ms: u64,
    timeout_ms: Option<u64>,
    extra_base_urls: Vec<String>,
) -> Result<Vec<stross_app::devices::ScannedDevice>, String> {
    let browse = std::time::Duration::from_millis(timeout_ms.unwrap_or(2000));
    let found = stross_core::discovery::Discovery::browse(browse)
        .await
        .map_err(|e| format!("mDNS 扫描失败: {e}"))?;
    let self_ips: Vec<String> = stross_core::net::local_ips()
        .into_iter()
        .map(|ip| ip.to_string())
        .collect();
    let probe = std::time::Duration::from_millis(probe_ms.max(100));
    let mut devices = stross_app::devices::scan(found, &self_ips, probe).await;
    // 手动地址并入（无 mDNS）：去重后追加探测条目
    let mut seen: std::collections::HashSet<String> = devices
        .iter()
        .map(|d| format!("{}:{}", d.ip, d.port))
        .collect();
    for base in extra_base_urls {
        let base = base.trim_end_matches('/').to_string();
        if let Some(d) = stross_app::devices::probe_base(&base, probe).await
            && seen.insert(format!("{}:{}", d.ip, d.port))
        {
            devices.push(d);
        }
    }
    Ok(devices)
}

/// 本机锚点中继（127.0.0.1）在线共享列表——「等待流接入」轮询用
/// （`beginAwaitMicStream` 等），不再由前端直接 fetch `/api/streams`。
#[tauri::command]
pub async fn anchor_streams(port: u16) -> Vec<stross_app::devices::StreamView> {
    use stross_core::relay::client as relay_http;
    stross_app::devices::to_views(
        relay_http::streams("127.0.0.1", port, std::time::Duration::from_millis(1500))
            .await
            .unwrap_or_default(),
    )
}

/// 手动地址可达性探测（`/api/streams` 是受控/普通中继都提供的只读端点；
/// 供「手动添加设备」校验地址用）。
#[tauri::command]
pub async fn probe_relay(base: String) -> bool {
    use stross_core::relay::client as relay_http;
    let url = format!("{}/api/streams", base.trim_end_matches('/'));
    relay_http::get_json::<serde_json::Value>(&url, std::time::Duration::from_secs(3))
        .await
        .is_ok()
}

/// L2 目录（远端节点设备 + 可订阅端点；类型化 `EndpointDir`）。
#[tauri::command]
pub async fn endpoint_ls(
    host: String,
    port: u16,
) -> Result<stross_proto::message::EndpointDir, String> {
    stross_app::fetch_directory(&host, port)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// 订阅远端文件端点并落盘到 `out_dir`（pull/push 全流程在库接口
/// `stross_app::subscribe_file`）。
#[tauri::command]
pub async fn endpoint_subscribe(
    app: tauri::AppHandle,
    state: State<'_, Arc<StrossApp>>,
    host: String,
    port: u16,
    endpoint_id: String,
    delivery: Option<String>,
    out_dir: String,
) -> Result<stross_app::SubscribeOutcome, String> {
    let app_state = state.inner().clone();
    let base = app
        .path()
        .app_data_dir()
        .unwrap_or_else(|_| std::env::temp_dir());
    let delivery = delivery.as_deref().map(|s| match s {
        "push" => stross_proto::message::Delivery::Push,
        _ => stross_proto::message::Delivery::Pull,
    });
    stross_app::subscribe_file(
        &app_state,
        &base,
        &host,
        port,
        &endpoint_id,
        delivery,
        std::path::Path::new(&out_dir),
    )
    .await
    .map_err(|e| format!("{e:#}"))
}

/// 向对端申请一次性接入凭证（B2.5 免粘贴：首次对端人工允许，之后信任免问）。
/// 旧语义（无端点）：`media` 指定申请媒体；返回授予（token / streamId）。
#[tauri::command]
pub async fn request_share_token(
    app: tauri::AppHandle,
    host: String,
    port: u16,
    media: Vec<stross_proto::message::MediaKind>,
) -> Result<stross_proto::message::ShareGrant, String> {
    let base = app
        .path()
        .app_data_dir()
        .unwrap_or_else(|_| std::env::temp_dir());
    let name = hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|_| "Stross 设备".into());
    let ident = stross_app::load_or_create_identity(&base, &name);
    let req = stross_proto::message::ShareRequest {
        device_id: ident.device_id,
        device_name: ident.device_name,
        endpoint_id: None,
        delivery_mode: None,
        relay_addr: None,
        share_token: None,
        media,
    };
    stross_app::request_grant(&host, port, &req)
        .await
        .map_err(|e| format!("{e:#}"))
}

// ---------------------------------------------------------------------------
// 凭证自动协商（权限自动化）：应答挂起请求 + 本机身份
// ---------------------------------------------------------------------------

/// 本机持久化身份（device_id / device_name；首次运行生成，之后稳定）。
#[tauri::command]
pub fn device_identity(app: tauri::AppHandle) -> stross_app::DeviceIdentity {
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
#[tauri::command]
pub fn negotiator_respond(
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
#[cfg(all(not(mobile), target_os = "linux"))]
#[tauri::command]
pub async fn firewall_status(
    state: State<'_, Arc<StrossApp>>,
) -> Result<crate::firewall::FirewallStatus, String> {
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
    let subnet = crate::firewall::lan_subnet(&ips);
    let mut status = crate::firewall::parse_ufw_verbose(&text);
    let (tcp, udp) = required_firewall_ports(&state);
    let required: Vec<&str> = tcp.iter().chain(udp.iter()).map(|s| s.as_str()).collect();
    status.missing = match subnet {
        Some(sub) => crate::firewall::missing_rules(
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
pub async fn firewall_allow(state: State<'_, Arc<StrossApp>>) -> Result<(), String> {
    let ips = stross_core::net::local_ips();
    let subnet = crate::firewall::lan_subnet(&ips)
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
