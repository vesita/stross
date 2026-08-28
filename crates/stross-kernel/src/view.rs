//! 展示视图（壳层只读；**内核产出，壳层不定义 wire 结构**）。
//!
//! 分层：线协议类型在 stross-proto；跨壳层复用的展示视图（设备卡片 / 推流
//! 状态 / 中继入口）由内核统一产出，壳层只做渲染与参数转译——避免各平台
//! 各写一份响应结构体（曾发生分层反转，见 layering 判据）。

use serde::Serialize;

use stross_proto::message::{DeviceSummary, RoleId, TransportId};

/// 应用信息（版本 / 平台 / ffmpeg 是否可用 / 本机 IP）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub version: String,
    /// "desktop" | "android"
    pub platform: String,
    pub ffmpeg: bool,
    pub ips: Vec<String>,
}

/// 摄像头 / 麦克风 / 系统声音设备清单。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceList {
    pub cameras: Vec<stross_media::devices::CameraDevice>,
    pub audio_inputs: Vec<String>,
    pub system_audio: Vec<String>,
}

/// 中继入口信息（mDNS 能力引导；本机中继或扫描结果共用）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayInfo {
    pub port: u16,
    pub urls: Vec<String>,
    /// 设备名（mDNS 能力引导 `name`；本机中继或缺失时为 `None`）。
    pub name: Option<String>,
    /// 类型（relay / sender / …）。
    pub kind: Option<String>,
    /// 角色（mDNS 能力引导 `roles`；枚举，序列化与字符串时代一致）。
    pub roles: Vec<RoleId>,
    /// 支持的传输（mDNS 能力引导 `transports`；序列化后与字符串时代一致）。
    pub transports: Vec<TransportId>,
    /// 中继 IP（本机中继时为 `None`，用 urls 展示）。
    pub ip: Option<String>,
    /// 端点框架 L1：该节点公开的设备清单摘要（id/kind/name/是否已公开；
    /// 本机 = 注册表快照，对端 = mDNS `DiscoveryInfo v2.devices` 解码）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<DeviceSummary>,
}

/// 推流启动结果。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartResult {
    pub relay_port: u16,
    pub watch_urls: Vec<String>,
    /// 实际流 id（内核签发，D4：与 session id 合一；接收端据此订阅）。
    pub stream_id: String,
}

/// 推流状态。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamStatus {
    pub running: bool,
    pub stream_id: Option<String>,
    pub title: Option<String>,
    pub relay_port: Option<u16>,
    pub started_at: Option<u64>,
}

/// 采集真实状态视图。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureStatusView {
    pub active: bool,
    pub started: bool,
    pub error: Option<String>,
}

/// 本机中继入口视图（含多网卡全部局域网 IP）。
///
/// 多网卡：列出全部局域网 IP 入口（无局域网 IP 时回退回环）。
pub fn relay_info(port: u16, devices: Vec<DeviceSummary>) -> RelayInfo {
    let urls = crate::transport::RelayUrl::http_entries(port);
    RelayInfo {
        port,
        urls,
        name: Some("Stross 本机中继".into()),
        kind: Some("relay".into()),
        roles: vec![RoleId::Sender, RoleId::Viewer, RoleId::Relay],
        transports: vec![
            TransportId::Ws,
            TransportId::WebRtc,
            TransportId::Srt,
            TransportId::Quic,
        ],
        ip: None,
        devices,
    }
}

/// 局域网可访问的中继入口（供其它设备连接数据面 / REST 端点）。
///
/// * 推到外部中继（`relay_url` 非回环地址）→ 直接指向该中继
/// * 本机中继（回环地址 / 未指定）→ 列出本机局域网 IP
pub fn watch_urls(relay_url: Option<&str>, relay_port: u16) -> Vec<String> {
    if let Some(url) = relay_url.and_then(crate::transport::RelayUrl::parse) {
        // 仅 ws 基址可直接反推 HTTP 入口（srt/quic 属 UDP 数据面端口，入口仍是本机 HTTP）
        if url.is_ws() && !url.is_loopback() {
            return vec![url.base_http()];
        }
    }
    crate::transport::RelayUrl::http_entries(relay_port)
}

/// 手机麦克风接入凭证视图（B2：电脑端签发后展示给手机）。
///
/// 线协议类型已收敛至 stross-proto：此处重导出保持
/// `stross_kernel::view::ShareTokenView` / `stross_kernel::ShareTokenView`
/// 路径兼容（GUI 命令层在用）。
pub use stross_proto::message::ShareTokenView;
