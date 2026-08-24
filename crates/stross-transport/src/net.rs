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

/// 第一个可用的局域网 IP（用于展示观看地址）。
pub fn local_ip() -> Option<IpAddr> {
    local_ips().into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ips_never_panic() {
        let _ = local_ips();
        let _ = local_ip();
    }
}
