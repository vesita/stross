//! GUI 命令面（Tauri `invoke` 薄命令层）——**桥**：前端 JS 不直接当协议
//! 客户端，一律经这里调 `stross_app` 库接口（docs/layering-architecture.md）。
//!
//! 与原桌面/Android 共用同一命令面；命令只做参数转译 + 错误 -> String，
//! 逻辑全部在库层（`Kernel` / `subscriber` / `devices` 等）。

use std::sync::Arc;

use stross_kernel::Kernel;
use tauri::{Manager, State};

use crate::NegotiatorHandle;

// ---------------------------------------------------------------------------
// 本机状态 / 发现 / 推流
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn app_info(state: State<'_, Arc<Kernel>>) -> stross_kernel::AppInfo {
    state.app_info()
}

/// 当前「可被发现」状态（mDNS 广播本机；以运行时状态为准）。
#[tauri::command]
pub fn discoverable_status(state: State<'_, Arc<Kernel>>) -> stross_kernel::Settings {
    stross_kernel::Settings {
        discoverable: state.discoverable(),
    }
}

/// 设置「可被发现」，并持久化到 settings.json（重启保持）。
#[tauri::command]
pub fn set_discoverable(
    app: tauri::AppHandle,
    state: State<'_, Arc<Kernel>>,
    on: bool,
) -> Result<(), String> {
    let base = app
        .path()
        .app_data_dir()
        .unwrap_or_else(|_| std::env::temp_dir());
    let mut stored = stross_kernel::load_settings(&base);
    stored.discoverable = on;
    stross_kernel::save_settings(&base, &stored);
    state.set_discoverable(on); // 立即生效（已锚定则开/停广播，未锚定记状态）
    Ok(())
}

#[tauri::command]
pub fn list_devices(state: State<'_, Arc<Kernel>>) -> stross_kernel::DeviceList {
    state.list_devices()
}

#[tauri::command]
pub async fn start_relay(
    state: State<'_, Arc<Kernel>>,
) -> Result<stross_kernel::RelayInfo, String> {
    // 固定端口（含 SRT/QUIC）：防火墙只放行已知端口（权限自动化）。
    // 端口真源：协议默认 [DEFAULT_PORT]（桌面）；Android GUI 固定
    // [GUI_PORT]（平台约定 AGENTS.md）——命令层按平台选，不定制逻辑。
    #[cfg(mobile)]
    let relay_port = stross_kernel::relay::GUI_PORT;
    #[cfg(not(mobile))]
    let relay_port = stross_kernel::relay::DEFAULT_PORT;
    state
        .start_relay_fixed(
            relay_port,
            stross_kernel::DEFAULT_SRT_PORT,
            stross_kernel::DEFAULT_QUIC_PORT,
            &stross_bridge::device_name_or("Stross 设备"),
        )
        .await
        .map_err(|e| e.to_user_string())
}

// ---------------------------------------------------------------------------
// 桥接：局域网扫描 / 目录 / 订阅 / 凭证申请（前端不再自写协议客户端）
// ---------------------------------------------------------------------------

/// 全量扫描局域网设备（与 CLI `stross devices` 同源
/// `stross_kernel::discovery::scan_lan`：mDNS 浏览 + HTTP 探测聚合 + 手动地址
/// 并入全在库层）。返回含在线共享 / SRT / QUIC 的完整视图，
/// 前端每轮刷新只需调它，不再自行 fetch `/api/info` `/api/streams`。
///
/// `extra_base_urls`：手动添加的地址（无 mDNS），一并探测并入结果。
#[tauri::command]
pub async fn scan_devices(
    probe_ms: u64,
    timeout_ms: Option<u64>,
    extra_base_urls: Vec<String>,
) -> Result<Vec<stross_kernel::discovery::ScannedDevice>, String> {
    let browse = std::time::Duration::from_millis(timeout_ms.unwrap_or(2000));
    let probe = std::time::Duration::from_millis(probe_ms);
    stross_kernel::discovery::scan_lan(browse, probe, extra_base_urls)
        .await
        .map_err(|e| format!("局域网扫描失败: {e}"))
}

/// 手动地址可达性探测（`/api/streams` 是受控/普通中继都提供的只读端点；
/// 供「手动添加设备」校验地址用）。
#[tauri::command]
pub async fn probe_relay(base: String) -> bool {
    use stross_kernel::relay::client as relay_http;
    let url = format!("{}/api/streams", base.trim_end_matches('/'));
    relay_http::get_json::<serde_json::Value>(&url, std::time::Duration::from_secs(3))
        .await
        .is_ok()
}

/// L2 目录（远端节点设备 + 可订阅端点；类型化 `EndpointDir`）。
/// `port` 缺省 = 协议约定协商端口（`DEFAULT_NEGOTIATOR_PORT`）——前端不持有
/// 端口常量（端口真源在库层）。
#[tauri::command]
pub async fn endpoint_ls(
    host: String,
    port: Option<u16>,
) -> Result<stross_proto::message::EndpointDir, String> {
    stross_kernel::fetch_directory(
        &host,
        port.unwrap_or(stross_kernel::DEFAULT_NEGOTIATOR_PORT),
    )
    .await
    .map_err(|e| format!("{e:#}"))
}

/// 通告本机设备为端点（端点框架：可见性 / delivery 由公开者声明，P1 1:1）。
#[tauri::command]
pub fn endpoint_publish(
    state: State<'_, Arc<Kernel>>,
    device_id: String,
    visibility: String,
    delivery: String,
) -> Result<stross_proto::message::EndpointManifest, String> {
    use stross_proto::message::{Delivery, Visibility};
    // wire 字符串 → 枚举的解析单一真源在 proto（from_wire），前端不重复定义
    let visibility = Visibility::from_wire(&visibility).unwrap_or(Visibility::Public);
    let delivery = Delivery::from_wire(&delivery).unwrap_or(Delivery::Pull);
    state
        .publish_endpoint(&device_id, visibility, delivery, None, None)
        .map_err(|e| e.to_user_string())
}

/// 取消通告端点（活动共享联动停止——取消通告 = 不再共享，踢出当前订阅者）。
#[tauri::command]
pub async fn endpoint_unpublish(
    state: State<'_, Arc<Kernel>>,
    endpoint_id: String,
) -> Result<(), String> {
    state
        .unpublish_endpoint(&endpoint_id)
        .await
        .map_err(|e| e.to_user_string())
}

/// 停止端点活动共享（本机端点树「停止共享」按钮：停流 + 拆除会话，保留通告）。
/// **async**：`stop_endpoint_share` → `stop_share_by_stream` → `tokio::spawn`
/// （优雅停流）需要 Tokio 运行时；同步命令跑在 GTK 主线程无 reactor，会
/// panic「there is no reactor running」。与 `negotiator_respond` 同理。
#[tauri::command]
pub async fn endpoint_stop_share(
    state: State<'_, Arc<Kernel>>,
    endpoint_id: String,
) -> Result<(), String> {
    state
        .stop_endpoint_share(&endpoint_id)
        .map_err(|e| e.to_user_string())
}

/// 本机目录（设备 + 已公开端点；本机节点卡片设备树渲染用）。
#[tauri::command]
pub fn local_catalog(state: State<'_, Arc<Kernel>>) -> stross_kernel::LocalCatalog {
    state.local_catalog()
}

/// 订阅远端媒体端点：握手返回观看入口（pull = 公开方中继；push = 本机中继），
/// 前端随后走既有 `start_receive` 实际观看/播放。
#[tauri::command]
pub async fn endpoint_subscribe_media(
    app: tauri::AppHandle,
    state: State<'_, Arc<Kernel>>,
    host: String,
    port: Option<u16>,
    endpoint_id: String,
    delivery: Option<String>,
) -> Result<stross_kernel::MediaSubscribeOutcome, String> {
    let base = app
        .path()
        .app_data_dir()
        .unwrap_or_else(|_| std::env::temp_dir());
    let delivery = delivery
        .as_deref()
        .and_then(stross_proto::message::Delivery::from_wire);
    stross_kernel::subscribe_media(
        &state.inner().clone(),
        &base,
        &host,
        port.unwrap_or(stross_kernel::DEFAULT_NEGOTIATOR_PORT),
        &endpoint_id,
        delivery,
    )
    .await
    .map_err(|e| format!("{e:#}"))
}

// ---------------------------------------------------------------------------
// 凭证自动协商（权限自动化）：应答挂起请求
// ---------------------------------------------------------------------------

/// 应答凭证协商请求（电脑端授权确认弹窗操作后调用）。
///
/// **必须 async**：`respond()` 会触发端点 `share()` → `spawn_media_share` 内部
/// `tokio::spawn`，需要 Tokio runtime 上下文（Rust 核心契约，docs/endpoint-model-v2.md §4 联动）。
/// 同步 `tauri::command` 在 GTK 主线程执行、无 reactor，会 panic
/// 「there is no reactor running」；async 命令跑在 tokio runtime 上。
#[tauri::command]
pub async fn negotiator_respond(
    state: State<'_, NegotiatorHandle>,
    req_id: String,
    allow: bool,
    remember: bool,
) -> Result<Option<stross_kernel::ShareGrant>, String> {
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
fn required_firewall_ports(state: &Kernel) -> (Vec<String>, Vec<String>) {
    // 实际端口（回退默认）：中继 WS + 凭证协商 TCP；SRT/QUIC UDP
    let (ws, srt, quic) = state.relay_ports().unwrap_or((
        stross_kernel::relay::DEFAULT_PORT,
        Some(stross_kernel::DEFAULT_SRT_PORT),
        Some(stross_kernel::DEFAULT_QUIC_PORT),
    ));
    let tcp = vec![
        format!("{ws}/tcp"),
        format!("{}/tcp", stross_kernel::DEFAULT_NEGOTIATOR_PORT),
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
    state: State<'_, Arc<Kernel>>,
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
    let ips = stross_kernel::net::local_ips();
    let subnet = crate::firewall::lan_subnet(&ips);
    let mut status = crate::firewall::parse_ufw_verbose(&text);
    let (tcp, udp) = required_firewall_ports(&state);
    let required: Vec<&str> = tcp
        .iter()
        .chain(udp.iter())
        .map(std::string::String::as_str)
        .collect();
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
pub async fn firewall_allow(state: State<'_, Arc<Kernel>>) -> Result<(), String> {
    let ips = stross_kernel::net::local_ips();
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
