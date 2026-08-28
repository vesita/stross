//! 局域网设备扫描聚合（mDNS 结果 + HTTP 探测 + 视图模型）——**内核层**。
//!
//! 分层（docs/layering-architecture.md）：扫描聚合对 CLI `devices`、`adb status`
//! 与 GUI 设备卡片是同一份逻辑——收敛在此，壳层只做**格式化/渲染**（中文
//! 标签、卡片布局各自完成，本模块只出结构化数据）。
//!
//! 输入：`crate::discovery::Discovery::browse` 原始结果 + 本机 IP + 探测超时；
//! 输出：[`ScannedDevice`] 列表（已去重、已过滤、已排序、已探测）。

use std::collections::HashMap;
use std::time::Duration;

use crate::discovery::{Discovered, Discovery};
use crate::net::local_ips;
use crate::relay::client as relay_http;
use serde::{Deserialize, Serialize};
use stross_proto::message::{
    DiscoveryInfo, EndpointSummary, MediaKind, RoleId, StreamInfo, TransportId,
};

/// 单条流的展示视图（video/audio 布尔投影；`adb status` 复用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamView {
    pub stream_id: String,
    pub title: String,
    pub video: bool,
    pub audio: bool,
    pub watchers: u32,
}

/// 流信息列表 → 展示视图。
pub fn to_views(list: Vec<StreamInfo>) -> Vec<StreamView> {
    list.into_iter()
        .map(|s| StreamView {
            stream_id: s.stream_id,
            title: s.title,
            video: s.video.is_some(),
            audio: s.audio.is_some(),
            watchers: s.watchers,
        })
        .collect()
}

/// 一台设备的聚合状态（发现 + 探测）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScannedDevice {
    pub name: String,
    pub ip: String,
    pub port: u16,
    /// 是否本机（按本机局域网 IP 匹配）。
    pub is_self: bool,
    /// 角色（共享 / 接收 / 中继）。
    pub roles: Vec<RoleId>,
    /// 可共享媒体（屏幕 / 麦克风 …）。
    pub media: Vec<MediaKind>,
    /// 支持的传输（WS / SRT / QUIC …）。
    pub transports: Vec<TransportId>,
    /// 端点框架 L1：该节点公开的端点清单摘要（id/kind/name/是否可挂载/是否已通告）。
    pub endpoints: Vec<EndpointSummary>,
    /// `/api/info` 可达（HTTP 探测成功）才为 true。
    pub online: bool,
    pub srt_port: Option<u16>,
    pub quic_port: Option<u16>,
    /// 该设备当前在线共享（点流可在 GUI 接收）。
    pub streams: Vec<StreamView>,
}

/// 扫描聚合：`browse` 结果 → 去重（按实例名，IPv4 优先）→ 解码 L1 摘要 →
/// 本机判定 → HTTP 探测（本机走回环，对端走广播 IP）→ 本机优先 + 名字排序。
///
/// `self_ips` 为本机局域网 IP 字符串集（探测/展示用）；纯函数输入输出，
/// 不依赖运行环境。
pub async fn scan(
    found: Vec<Discovered>,
    self_ips: &[String],
    probe: Duration,
) -> Vec<ScannedDevice> {
    // 同一实例可能按 A/AAAA 记录各触发一次 ServiceResolved——按实例名去重，
    // 地址优先取 IPv4（发现层已剔除 link-local；IPv6 前缀跨设备常不通）。
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut devices: Vec<ScannedDevice> = Vec::new();
    for d in found {
        if let Some(&idx) = seen.get(&d.instance) {
            if devices[idx].ip.contains(':') && d.ip.is_ipv4() {
                devices[idx].ip = d.ip.to_string();
            }
            continue;
        }
        seen.insert(d.instance.clone(), devices.len());
        let info = DiscoveryInfo::from_txt(&d.txt);
        let ip = d.ip.to_string();
        let mut dev = ScannedDevice {
            name: info
                .as_ref()
                .map(|i| i.name.clone())
                .unwrap_or_else(|| d.instance.clone()),
            port: d.port,
            ip: ip.clone(),
            is_self: self_ips.contains(&ip),
            roles: info.as_ref().map(|i| i.roles.clone()).unwrap_or_default(),
            media: info.as_ref().map(|i| i.media.clone()).unwrap_or_default(),
            transports: info
                .as_ref()
                .map(|i| i.transports.clone())
                .unwrap_or_default(),
            endpoints: info
                .as_ref()
                .map(|i| i.endpoints.clone())
                .unwrap_or_default(),
            online: false,
            srt_port: None,
            quic_port: None,
            streams: Vec::new(),
        };
        // 探测地址：本机走回环（局域网 IP 在部分网络栈上不可自连），
        // 对端走其广播 IP；两个请求独立超时，互不拖累
        let probe_ip = if dev.is_self {
            "127.0.0.1".to_string()
        } else {
            ip
        };
        if let Ok(resp) = relay_http::info(&probe_ip, d.port, probe).await {
            dev.online = true;
            dev.srt_port = resp.srt_port;
            dev.quic_port = resp.quic_port;
        }
        if let Ok(list) = relay_http::streams(&probe_ip, d.port, probe).await {
            dev.streams = to_views(list);
        }
        devices.push(dev);
    }
    // 本机优先，其余按名字排序——输出稳定，脚本可比对
    devices.sort_by(|a, b| b.is_self.cmp(&a.is_self).then(a.name.cmp(&b.name)));
    devices
}

/// 完整局域网扫描（CLI `devices` 与 GUI `scan_devices` 命令共用的**唯一**入口）：
/// mDNS 浏览 → 本机 IP → 探测聚合 → 手动地址并入。
///
/// 分层（docs/layering-architecture.md）：browse + `local_ips` + 去重合并
/// 全部收敛在此，壳层（CLI 命令 / Tauri 命令）只做参数转译，禁止各自拼装
/// 聚合流程（曾出现在 GUI `scan_devices` 命令层）。
///
/// `browse` mDNS 浏览窗口；`probe` 每设备 HTTP 探测超时（下限 100ms，避免
/// 壳层误传小值打爆网络）；`extra_base_urls` 手动添加的地址（无 mDNS），
/// 一并探测并入（按 `ip:port` 去重）。
pub async fn scan_lan(
    browse: Duration,
    probe: Duration,
    extra_base_urls: Vec<String>,
) -> anyhow::Result<Vec<ScannedDevice>> {
    let found = Discovery::browse(browse).await?;
    let self_ips: Vec<String> = local_ips().into_iter().map(|ip| ip.to_string()).collect();
    let probe = probe.max(Duration::from_millis(100));
    let mut devices = scan(found, &self_ips, probe).await;
    // 手动地址并入（无 mDNS）：去重后追加探测条目
    let mut seen: std::collections::HashSet<String> = devices
        .iter()
        .map(|d| format!("{}:{}", d.ip, d.port))
        .collect();
    for base in extra_base_urls {
        let base = base.trim_end_matches('/').to_string();
        if let Some(d) = probe_base(&base, probe).await
            && seen.insert(format!("{}:{}", d.ip, d.port))
        {
            devices.push(d);
        }
    }
    Ok(devices)
}

/// 手动地址探测（无 mDNS 的设备）：解析 `http://host:port` 基址，探测
/// 在线 / SRT/QUIC / 在线共享。返回 `None` = 地址非法（无法探测）。
/// GUI「手动添加设备」与 CLI 可共用——不再各自实现探测客户端。
pub async fn probe_base(base: &str, probe: Duration) -> Option<ScannedDevice> {
    let rest = base
        .strip_prefix("http://")
        .or_else(|| base.strip_prefix("https://"))?;
    let host_port = rest.split('/').next()?;
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().ok()?),
        None => (host_port.to_string(), 80),
    };
    let mut dev = ScannedDevice {
        name: host_port.to_string(),
        ip: host.clone(),
        port,
        is_self: false,
        roles: vec![],
        media: vec![],
        transports: vec![],
        endpoints: vec![],
        online: false,
        srt_port: None,
        quic_port: None,
        streams: Vec::new(),
    };
    if let Ok(resp) = relay_http::info(&host, port, probe).await {
        dev.online = true;
        dev.srt_port = resp.srt_port;
        dev.quic_port = resp.quic_port;
    }
    if let Ok(list) = relay_http::streams(&host, port, probe).await {
        dev.streams = to_views(list);
    }
    Some(dev)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn entry(instance: &str, ip: &str, port: u16, txt: &[(String, String)]) -> Discovered {
        Discovered {
            instance: instance.to_string(),
            ip: ip.parse::<IpAddr>().unwrap(),
            port,
            txt: txt.to_vec(),
        }
    }

    /// 无网卡 / 无对端环境下：空输入 → 空输出，不 panic（探测不触发）。
    #[tokio::test]
    async fn scan_empty_never_panics() {
        let out = scan(vec![], &[], Duration::from_millis(100)).await;
        assert!(out.is_empty());
    }

    /// 同实例双地址（A/AAAA）按实例去重，IPv4 优先。
    #[tokio::test]
    async fn scan_dedupes_by_instance_preferring_ipv4() {
        let txt = DiscoveryInfo {
            v: DiscoveryInfo::VERSION,
            name: "电脑".into(),
            roles: vec![RoleId::Sender],
            media: vec![MediaKind::Screen],
            transports: vec![TransportId::Ws],
            codecs: vec![],
            endpoints: vec![],
        }
        .to_txt();
        let found = vec![
            entry("pc-1", "192.168.11.61", 18777, &txt),
            entry("pc-1", "fe80::1", 18777, &txt),
        ];
        // 探测超时只有 5ms：两台都不会 online，但去重/排序/字段可断言
        let out = scan(found, &["192.168.11.61".into()], Duration::from_millis(5)).await;
        assert_eq!(out.len(), 1, "同实例应去重");
        assert_eq!(out[0].ip, "192.168.11.61", "IPv4 优先");
        assert!(out[0].is_self);
    }

    #[test]
    fn stream_info_maps_to_view() {
        let v = to_views(vec![StreamInfo {
            stream_id: "s1".into(),
            title: "t".into(),
            video: None,
            audio: None,
            started_at: 1,
            watchers: 2,
        }]);
        assert_eq!(v[0].watchers, 2);
        assert!(!v[0].video);
    }
}
