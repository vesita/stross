//! 展示视图构造（壳层只读；**内核产出，壳层不定义 wire 结构**）。
//!
//! 分层：线协议类型在 stross-proto；跨壳层复用的**类型定义**（设备卡片 / 推流
//! 状态 / 中继入口 / 控制面载荷）全部收敛在 stross-types（应用契约层单一真源），
//! 本模块只保留**构造帮助函数**（`relay_info` / `watch_urls`），并为既有
//! `stross_kernel::view::*` 调用点做兼容重导出。
//!
//! 新代码请优先引用 `stross_types`（或顶层重导出 `stross_kernel::XxxView`）。

use stross_proto::message::{EndpointSummary, RoleId, TransportId};

pub use stross_types::*;

/// 本机中继入口视图（含多网卡全部局域网 IP）。
///
/// 多网卡：列出全部局域网 IP 入口（无局域网 IP 时回退回环）。
/// `name`：本机设备名（与 mDNS 广播名一致，壳层注入）。
pub fn relay_info(port: u16, name: &str, endpoints: Vec<EndpointSummary>) -> RelayInfo {
    let urls = crate::transport::RelayUrl::http_entries(port);
    RelayInfo {
        port,
        urls,
        name: Some(name.into()),
        kind: Some("relay".into()),
        roles: vec![RoleId::Sender, RoleId::Viewer, RoleId::Relay],
        transports: vec![
            TransportId::Ws,
            TransportId::WebRtc,
            TransportId::Srt,
            TransportId::Quic,
        ],
        ip: None,
        endpoints,
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
