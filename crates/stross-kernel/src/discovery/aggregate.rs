//! 局域网设备扫描聚合（mDNS 结果 + HTTP 探测 + 视图模型）——**内核层**（discovery 子系统）。
//!
//! 分层（docs/layering-architecture.md）：扫描聚合对 CLI `devices`、`adb status`
//! 与 GUI 设备卡片是同一份逻辑——收敛在此，壳层只做**格式化/渲染**（中文
//! 标签、卡片布局各自完成，本模块只出结构化数据）。
//!
//! 输入：`super::Discovery::browse` 原始结果 + 本机 IP + 探测超时；
//! 输出：[`ScannedDevice`] 列表（已去重、已过滤、已排序、已探测）。

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use futures_util::StreamExt;

use super::{Discovered, Discovery};
use crate::net::{is_fake_or_link_local, local_ips};
use crate::relay::client as relay_http;
use serde::{Deserialize, Serialize};
use stross_proto::message::{
    DiscoveryInfo, EndpointSummary, MediaKind, RoleId, StreamInfo, TransportId,
};
use utoipa::ToSchema;

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
                .map_or_else(|| d.instance.clone(), |i| i.name.clone()),
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
/// mDNS 浏览 → 本机 IP → 探测聚合 → （mDNS 失效时）子网单播扫描回退 →
/// 手动地址并入。
///
/// 分层（docs/layering-architecture.md）：browse + `local_ips` + 去重合并
/// 全部收敛在此，壳层（CLI 命令 / Tauri 命令）只做参数转译，禁止各自拼装
/// 聚合流程（曾出现在 GUI `scan_devices` 命令层）。
///
/// `browse` mDNS 浏览窗口；`probe` 每设备 HTTP 探测超时（下限 100ms，避免
/// 壳层误传小值打爆网络）；`extra_base_urls` 手动添加的地址（无 mDNS），
/// 一并探测并入（按 `ip:port` 去重）。
///
/// **mDNS 失效回退**：当 mDNS 浏览未发现任何**远端**设备（路由只掐下行多播、
/// 单播仍通——见 docs/mdns-android-finding-debug.md §8.2）时，自动触发纯单播
/// 子网扫描（[`subnet_scan`]），保证在「广播不可用」的网络下仍能发现对端。
/// 只在 mDNS 零结果才扫描，避免每次刷新都打满网卡。
pub async fn scan_lan(
    browse: Duration,
    probe: Duration,
    extra_base_urls: Vec<String>,
) -> anyhow::Result<Vec<ScannedDevice>> {
    let raw_ips = local_ips();
    let self_ips: Vec<IpAddr> = raw_ips.clone();
    let self_ip_strings: Vec<String> = raw_ips.iter().map(|ip| ip.to_string()).collect();
    let probe = probe.max(Duration::from_millis(100));
    let found = Discovery::browse(browse).await?;
    let mut devices = scan(found, &self_ip_strings, probe).await;
    // mDNS 零远端 → 子网单播扫描回退（纯单播，与组播/广播无关）
    if !devices.iter().any(|d| !d.is_self) {
        tracing::info!("mDNS 零远端设备，触发子网单播扫描回退");
        let scanned = subnet_scan(&self_ips, &self_ip_strings, probe).await;
        tracing::info!("子网扫描回退发现 {} 台设备", scanned.len());
        devices.extend(scanned);
    }
    // 手动地址并入（无 mDNS）：去重后追加探测条目
    let mut seen: HashSet<String> = devices
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

/// 统一发现清单（`GET /api/discovery`，监听于发现权威端口 [`DISCOVERY_PORT`]）：
/// 每台设备对外只暴露**一个权威节点**——身份 + 能力 + 真实中继入口端口。
/// mDNS 与子网扫描都据此收敛到**同一台设备同一个 `relay_port`**
/// （docs/mdns-android-finding-debug.md §8.3-3：mDNS 与单播兜底应指向同一节点，
/// 降低用户认知成本）。`relayPort` 是设备连接/展示节点，`srtPort/quicPort` 为数据面端口。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryResp {
    pub device_id: String,
    pub name: String,
    /// 中继 HTTP/WS 入口端口（本设备连接/展示节点 = ScannedDevice.port）。
    pub relay_port: u16,
    pub srt_port: Option<u16>,
    pub quic_port: Option<u16>,
    pub roles: Vec<RoleId>,
    pub media: Vec<MediaKind>,
    pub transports: Vec<TransportId>,
    pub endpoints: Vec<EndpointSummary>,
}

/// 发现权威端口：协商/发现服务监听于此（桌面与 Android GUI 一致），
/// 是 mDNS 与子网扫描共同收敛的节点入口。**与协商端口是同一端口**，真源在
/// [`stross_types::ports::NEGOTIATOR_DISCOVERY`]（此处仅别名，消除局部重复定义）。
pub use stross_types::ports::NEGOTIATOR_DISCOVERY as DISCOVERY_PORT;

/// 子网扫描回退时探测的发现权威端口：单一 [`DISCOVERY_PORT`]，读到
/// [`DiscoveryResp`] 后以其 `relay_port` 作设备节点——不再硬编码多个中继端口，
/// 故能发现自定义中继端口设备，且与 mDNS 指向同一节点。
/// 子网扫描并行探测的并发上限（控制打满网卡；扫描是回退路径，不求快、求稳）。
const SCAN_CONCURRENCY: usize = 64;

/// 从本机局域网 IPv4 推得待扫描的子网（/24 网段地址，去重、升序）。
///
/// 剔除 fake-IP（Clash TUN 198.18/15）、链路本地（169.254/16）与回环——这些
/// 都不是可拨号的局域网网段（AGENTS.md §6 已知坑；`local_ips()` 会带出
/// Android 的 rndis0/vgate0 与 PC 的 TUN fake-IP，此处一律不让它们进入扫描）。
fn scan_subnets(self_ips: &[IpAddr]) -> Vec<Ipv4Addr> {
    let mut nets: Vec<Ipv4Addr> = self_ips
        .iter()
        .filter_map(|ip| {
            let IpAddr::V4(v4) = ip else {
                return None;
            };
            if v4.is_loopback() || is_fake_or_link_local(ip) {
                return None;
            }
            Some(Ipv4Addr::new(
                v4.octets()[0],
                v4.octets()[1],
                v4.octets()[2],
                0,
            ))
        })
        .collect();
    nets.sort_unstable();
    nets.dedup();
    nets
}

/// 本机各 /24 网段的全部候选主机（.1–.254，跨子网去重；子网已去重）。
fn scan_hosts(self_ips: &[IpAddr]) -> Vec<Ipv4Addr> {
    let mut hosts = Vec::new();
    for net in scan_subnets(self_ips) {
        let [a, b, c, _] = net.octets();
        for h in 1u8..=254 {
            hosts.push(Ipv4Addr::new(a, b, c, h));
        }
    }
    hosts
}

/// 对单个候选主机单播探测发现权威端口（[`DISCOVERY_PORT`] 18779）：读到
/// [`DiscoveryResp`] 即以其中继入口端口构造 [`ScannedDevice`]（在线）；
/// 读不到（未锚定 / 非本框架节点 / 不在发现端口）返回 `None`。
///
/// 以清单的 `relay_port` 作为设备**连接/展示节点**，再用同一端口查询中继
/// `/api/info` + `/api/streams` 填充在线/数据面端口/在线共享（与 mDNS 路径
/// `scan` 的语义一致——两路径最终指向同一设备同一 `relay_port`）。
/// 每端口超时截断到 300ms（扫描是打通的快检；对端中继在局域网内应远快于此）。
async fn scan_probe_host(host: &str, probe: Duration) -> Option<ScannedDevice> {
    let fast = probe.min(Duration::from_millis(300));
    let disc: DiscoveryResp = relay_http::get_json(
        &format!("http://{host}:{DISCOVERY_PORT}/api/discovery"),
        fast,
    )
    .await
    .ok()?;
    let mut dev = ScannedDevice {
        name: disc.name,
        ip: host.to_string(),
        port: disc.relay_port,
        is_self: false,
        roles: disc.roles,
        media: disc.media,
        transports: disc.transports,
        endpoints: disc.endpoints,
        online: false,
        srt_port: disc.srt_port,
        quic_port: disc.quic_port,
        streams: Vec::new(),
    };
    if let Ok(resp) = relay_http::info(host, dev.port, fast).await {
        dev.online = true;
        dev.srt_port = resp.srt_port;
        dev.quic_port = resp.quic_port;
    }
    if let Ok(list) = relay_http::streams(host, dev.port, fast).await {
        dev.streams = to_views(list);
    }
    Some(dev)
}

/// 子网单播扫描回退（mDNS 组播不可用时）：对本机各 /24 网段主机并发单播
/// 探测中继入口端口，命中即聚合成 [`ScannedDevice`]。
///
/// **纯单播，不依赖组播/广播**——适配「路由只掐下行多播、单播仍通」的网络
/// （docs/mdns-android-finding-debug.md §8.2：手机收不到下行多播、单播双向正常，
/// 正是这种网络 mDNS 失效但本方案可用）。`self_ip_strings` 用于把扫到的本机标为
/// `is_self`（与 mDNS 路径 `scan` 的语义一致）。
async fn subnet_scan(
    self_ips: &[IpAddr],
    self_ip_strings: &[String],
    probe: Duration,
) -> Vec<ScannedDevice> {
    let hosts = scan_hosts(self_ips);
    if hosts.is_empty() {
        return Vec::new();
    }
    futures_util::stream::iter(hosts)
        .map(|ip| {
            let ipstr = ip.to_string();
            let selfs = self_ip_strings.to_vec();
            async move {
                let mut dev = scan_probe_host(&ipstr, probe).await?;
                dev.is_self = selfs.contains(&ipstr);
                Some(dev)
            }
        })
        .buffer_unordered(SCAN_CONCURRENCY)
        .filter_map(|d| async move { d })
        .collect::<Vec<_>>()
        .await
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

    fn ips(cidrs: &[&str]) -> Vec<IpAddr> {
        cidrs.iter().map(|s| s.parse().unwrap()).collect()
    }

    #[test]
    fn scan_subnets_dedups_and_filters_unroutable() {
        // rndis0/vgate0/TUN fake-IP/链路本地/回环均不得进入扫描网段
        let out = scan_subnets(&ips(&[
            "192.168.11.61",
            "192.168.11.60",  // 同 /24 → 去重
            "10.159.157.104", // USB 共享网 → 保留
            "198.18.0.5",     // Clash TUN fake-IP → 剔除
            "169.254.3.4",    // APIPA → 剔除
            "127.0.0.1",      // 回环 → 剔除
            "fe80::1",        // IPv6 → 忽略（只扫 IPv4）
        ]));
        assert_eq!(
            out,
            vec![
                Ipv4Addr::new(10, 159, 157, 0),
                Ipv4Addr::new(192, 168, 11, 0),
            ]
        );
    }

    #[test]
    fn scan_hosts_enumerates_24_net_single_subnet() {
        let out = scan_hosts(&ips(&["192.168.11.61"]));
        assert_eq!(out.len(), 254, "/24 主机数为 254");
        assert_eq!(out[0], Ipv4Addr::new(192, 168, 11, 1));
        assert_eq!(out[253], Ipv4Addr::new(192, 168, 11, 254));
        assert!(!out.contains(&Ipv4Addr::new(192, 168, 11, 0)));
    }

    #[test]
    fn scan_hosts_dedups_across_subnets() {
        // 两个 IP 同 /24 → 只扫一份（不重复 254 次）
        let out = scan_hosts(&ips(&["192.168.11.61", "192.168.11.2"]));
        assert_eq!(out.len(), 254);
    }

    #[test]
    fn scan_hosts_empty_when_no_usable_public_ipv4() {
        // 只有回环 / fake-IP / IPv6 → 无网段可扫（不会自动扫描占位网段）
        assert!(scan_hosts(&ips(&["127.0.0.1", "198.18.0.5", "fe80::1"])).is_empty());
    }
}
