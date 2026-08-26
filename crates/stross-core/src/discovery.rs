//! mDNS 服务发现（可选 feature `discovery`）。
//!
//! 中继/推流端用 `_stross._tcp` 广播自己，局域网内其它设备可以发现中继入口。
//! 借鉴 [mdns-sd](https://crates.io/crates/mdns-sd) 的用法。
//!
//! 能力引导（F1.2）：TXT 单 key（`stross`）承载整个 [`DiscoveryInfo`]（JSON），
//! 见 [`stross_proto::message::DiscoveryInfo`]——注册侧传结构体，浏览侧解码结构体。

use std::net::IpAddr;
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use stross_proto::message::DiscoveryInfo;

/// mDNS 服务类型。
pub const SERVICE_TYPE: &str = "_stross._tcp.local.";

/// 一个被发现的服务实例。
#[derive(Debug, Clone)]
pub struct Discovered {
    pub instance: String,
    pub ip: IpAddr,
    pub port: u16,
    pub txt: Vec<(String, String)>,
}

/// mDNS 广播句柄。
pub struct Discovery {
    daemon: Option<ServiceDaemon>,
}

impl Discovery {
    /// 以 `instance` 名义广播服务（能力描述见 [`DiscoveryInfo`]）。
    ///
    /// `ips` 为要广播的本机局域网地址：**多网卡时全部传入**（一次注册
    /// 即携带全部 A/AAAA 记录，各网卡网段都能扫描到本机）；空列表回退
    /// 回环地址（至少保证本机可发现，行为与单 IP 时代一致）。
    pub fn start(
        instance: &str,
        ips: &[IpAddr],
        port: u16,
        info: &DiscoveryInfo,
    ) -> anyhow::Result<Self> {
        let daemon = ServiceDaemon::new()?;
        let hostname = hostname::get().unwrap_or_else(|_| "stross".into());
        let host = format!("{}.local.", hostname.to_string_lossy());
        // mdns-sd 0.21：TXT 属性走 IntoTxtProperties（HashMap<String, String>）；
        // 能力描述由 DiscoveryInfo 单 key JSON 编码（新增字段零维护）
        let props: std::collections::HashMap<String, String> = info.to_txt().into_iter().collect();
        // 多网卡广播：ServiceInfo::new 支持 AsIpAddrs（&[IpAddr]），
        // 一次注册携带全部地址记录
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            instance,
            &host,
            broadcast_addrs(ips).as_slice(),
            port,
            props,
        )
        .map_err(|e| anyhow::anyhow!("ServiceInfo: {e}"))?;
        // 显式地址为准：**不启用 enable_addr_auto()**。
        // 原因（Android 真机实测）：mdns-sd 的 `if_addrs::get_if_addrs()` 会枚举到
        // dummy0/ifb0/ifb1 等虚拟接口及其 fe80 地址，auto 覆盖后把真实 wlan0 IPv4
        // （如 192.168.11.60）挤掉，导致对端扫到手机只有不可达的 fe80 地址。
        // 调用方 `local_ips()` 已返回真实局域网 IPv4，显式传入即可；
        // 代价：WiFi 切换时需重新 start_relay 再注册（本应用每次锚定都会重走）。
        daemon.register(info)?;
        Ok(Self {
            daemon: Some(daemon),
        })
    }

    /// 在 `timeout` 内浏览局域网内的 Stross 服务。
    pub async fn browse(timeout: Duration) -> anyhow::Result<Vec<Discovered>> {
        let daemon = ServiceDaemon::new()?;
        let receiver = daemon.browse(SERVICE_TYPE)?;
        let deadline = tokio::time::Instant::now() + timeout;
        let mut out = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let Ok(event) = tokio::time::timeout(remaining, receiver.recv_async()).await else {
                break;
            };
            match event {
                Ok(ServiceEvent::ServiceResolved(info)) => {
                    // mdns-sd 0.21：地址为 HashSet<ScopedIp>（带接口信息）。
                    // 选址策略（真机实测定稿）：
                    // 1. link-local（fe80::/10、169.254/16）无 scope 不可达，
                    //    且 `enable_addr_auto` 会把网卡全地址（含 fe80）带进广播，
                    //    必须剔除——否则网格出现「点不开」的设备卡片；
                    // 2. **优先 IPv4**：双栈 WiFi 上不同设备的 IPv6 前缀常不通
                    //    （真机：PC 240e:3a8… 与手机 240e:579… 互不可达），
                    //    IPv4 才是同网段的可靠路径；IPv6 全局地址仅作纯 IPv6
                    //    局域网的后备。
                    let reachable: Vec<IpAddr> = info
                        .get_addresses()
                        .iter()
                        .map(|s| s.to_ip_addr())
                        .filter(|i| match i {
                            IpAddr::V4(v4) => !v4.is_link_local() && !v4.is_unspecified(),
                            IpAddr::V6(v6) => {
                                v6.segments()[0] & 0xffc0 != 0xfe80 && !v6.is_unspecified()
                            }
                        })
                        .collect();
                    tracing::debug!(
                        "mDNS 解析到服务 {}，地址 {:?}",
                        info.get_fullname(),
                        info.get_addresses()
                            .iter()
                            .map(|s| s.to_ip_addr().to_string())
                            .collect::<Vec<_>>(),
                    );
                    let ip = reachable
                        .iter()
                        .find(|i| i.is_ipv4())
                        .or_else(|| reachable.first());
                    if let Some(ip) = ip {
                        out.push(Discovered {
                            instance: info.get_fullname().to_string(),
                            ip: *ip,
                            port: info.get_port(),
                            txt: info
                                .get_properties()
                                .iter()
                                .map(|p| (p.key().to_string(), p.val_str().to_string()))
                                .collect(),
                        });
                    }
                }
                Ok(ServiceEvent::ServiceRemoved(_, _)) => {}
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let _ = daemon.shutdown();
        Ok(out)
    }

    /// 停止广播。
    pub fn stop(&mut self) {
        if let Some(d) = self.daemon.take() {
            let _ = d.shutdown();
        }
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 待广播的地址列表：多网卡全部保留；**过滤 link-local**（fe80::/10 与
/// 169.254/16）：无 scope 的链路本地地址对其它主机不可达，广播出去只会
/// 制造「点不开」的设备卡片（真机实测发现）。空列表回退回环地址。
///
/// 抽成纯函数便于单测（`ServiceInfo` 构造需要真实 mDNS socket，不在此测试）。
fn broadcast_addrs(ips: &[IpAddr]) -> Vec<IpAddr> {
    if ips.is_empty() {
        return vec![IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)];
    }
    let filtered: Vec<IpAddr> = ips
        .iter()
        .copied()
        .filter(|ip| match ip {
            IpAddr::V4(v4) => !v4.is_link_local(),
            // fe80::/10（前 10 位 1111111010）
            IpAddr::V6(v6) => v6.segments()[0] & 0xffc0 != 0xfe80,
        })
        .collect();
    if filtered.is_empty() {
        // 全是 link-local（如仅 fe80 的纯 IPv6 主机）：原样广播兜底，
        // 否则本机将完全不可被发现；对端会自行判断可达性
        ips.to_vec()
    } else {
        filtered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_all_ips_when_multiple() {
        // 多网卡：全部地址都应广播（已知问题②回归：此前只取第一个 IP）
        let ips = vec![
            IpAddr::V4("192.168.1.10".parse().unwrap()),
            IpAddr::V4("10.0.0.5".parse().unwrap()),
        ];
        assert_eq!(broadcast_addrs(&ips), ips);
    }

    #[test]
    fn broadcast_single_ip_passthrough() {
        let ips = vec![IpAddr::V4("192.168.1.10".parse().unwrap())];
        assert_eq!(broadcast_addrs(&ips), ips);
    }

    #[test]
    fn broadcast_empty_falls_back_to_loopback() {
        // 空列表（无局域网地址）→ 回退回环，行为与单 IP 时代一致
        assert_eq!(
            broadcast_addrs(&[]),
            vec![IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)]
        );
    }

    #[test]
    fn broadcast_filters_link_local() {
        // link-local（fe80::/10、169.254/16）不可达，必须过滤（真机实测：
        // 手机扫描到 PC 的 fe80 条目 → 点卡片连不上）
        let ips = vec![
            IpAddr::V4("192.168.1.10".parse().unwrap()),
            IpAddr::V4("169.254.3.4".parse().unwrap()), // APIPA
            IpAddr::V6("fe80::835:6e70:4ba6:eb01".parse().unwrap()),
            IpAddr::V6("240e:3a8:4c9f:2401:abcd::1".parse().unwrap()),
        ];
        assert_eq!(
            broadcast_addrs(&ips),
            vec![
                IpAddr::V4("192.168.1.10".parse().unwrap()),
                IpAddr::V6("240e:3a8:4c9f:2401:abcd::1".parse().unwrap()),
            ]
        );
    }

    #[test]
    fn broadcast_all_fe80_falls_back_to_original() {
        // 极端场景：全是 link-local（纯 IPv6 链路）→ 原样广播兜底，避免不可发现
        let ips = vec![IpAddr::V6("fe80::1".parse().unwrap())];
        assert_eq!(broadcast_addrs(&ips), ips);
    }
}
