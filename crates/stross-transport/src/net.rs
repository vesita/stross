//! 网络工具：获取本机局域网 IP。

use std::net::IpAddr;

/// 列出所有非回环的局域网 IP。
pub fn local_ips() -> Vec<IpAddr> {
    let Ok(interfaces) = local_ip_address::list_afinet_netifas() else {
        return Vec::new();
    };
    interfaces
        .into_iter()
        .map(|(_, ip)| ip)
        .filter(|ip| !ip.is_loopback())
        .collect()
}

/// 第一个可用的局域网 IP（用于展示中继入口）。
pub fn local_ip() -> Option<IpAddr> {
    local_ips().into_iter().next()
}

/// IP 是否「不可对外广告」：Clash/Mihomo TUN 的 fake-IP 段（198.18.0.0/15，
/// 路由表占位、连不通）与链路本地（169.254/16）。IPv6 一律视为可广告
/// （子网前缀未知，交给对端选址判断）。
pub const fn is_fake_or_link_local(ip: &IpAddr) -> bool {
    let IpAddr::V4(v4) = ip else {
        return false;
    };
    let o = v4.octets();
    (o[0] == 198 && o[1] == 18) || (o[0] == 169 && o[1] == 254)
}

/// 广告用本机 IP（出站推 / 订阅方中继基址等对外地址）：优先第一个
/// **非 fake-IP、非链路本地** 的 IPv4，全不合格回退回环。
///
/// 与发现层选址同源决策（AGENTS.md §6 已知坑：fake-IP/TUN 必须排除）。
pub fn advertise_ip() -> String {
    for ip in local_ips() {
        let IpAddr::V4(v4) = ip else {
            continue;
        };
        if !is_fake_or_link_local(&IpAddr::V4(v4)) {
            return ip.to_string();
        }
    }
    "127.0.0.1".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ips_never_panic() {
        let _ = local_ips();
        let _ = local_ip();
        let _ = advertise_ip(); // 无网卡环境也应回退回环，不 panic
    }

    #[test]
    fn fake_and_link_local_detection() {
        use std::net::{Ipv4Addr, Ipv6Addr};
        assert!(is_fake_or_link_local(&IpAddr::V4(Ipv4Addr::new(
            198, 18, 0, 1
        ))));
        assert!(is_fake_or_link_local(&IpAddr::V4(Ipv4Addr::new(
            169, 254, 3, 4
        ))));
        assert!(!is_fake_or_link_local(&IpAddr::V4(Ipv4Addr::new(
            192, 168, 11, 61
        ))));
        assert!(!is_fake_or_link_local(&IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }
}
