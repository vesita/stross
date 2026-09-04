//! 防火墙自动放行（权限自动化）：ufw 自检 + polkit 一键放行。
//!
//! # 背景
//!
//! 跨设备共享需要放行本机中继（WS/SRT/QUIC）与凭证协商端口；`ufw` 默认
//! `deny (incoming)` 时，其它设备无法直连。此前需要用户手敲
//! `sudo ufw allow from 192.168.11.0/24`，本模块把这件事变成：
//!
//! 1. `firewall_status()`：只读自检（`ufw status verbose`），返回缺失放行
//!    （无副作用，任何用户可执行）
//! 2. `firewall_allow()`：经 polkit（`pkexec`）弹**一次**系统授权，自动添加
//!    精确规则（仅放行 Stross 端口 × 本机局域网子网，比整个网段全放更窄）
//!
//! # 边界
//!
//! * 仅 Linux（`ufw` 为 Ubuntu/Debian 系防火墙）；其他平台命令返回未支持
//! * 端口固定化是前提：SRT/QUIC 默认固定（`stross_kernel::DEFAULT_SRT/QUIC_PORT`），
//!   被占用回退随机时按**实际端口**生成规则
//! * `pkexec` 需要图形 polkit 代理（KDE/GNOME 桌面自带）；无代理时返回
//!   可读错误并提示手动命令，绝不在应用内提权

use std::net::IpAddr;

/// 一条 `ufw` 放行规则（表格行解析结果）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FirewallRule {
    /// `port/proto`（如 `18777/tcp`）。
    pub port_proto: String,
    /// 来源（`Anywhere` / `192.168.11.0/24` 原样）。
    pub from: String,
}

/// 防火墙自检结果（序列化给前端展示 / 决定是否提示一键放行）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FirewallStatus {
    /// ufw 是否启用。
    pub ufw_active: bool,
    /// 入站默认是否拒绝（`ufw status verbose` 的 `Default: deny (incoming)`）。
    pub default_deny_incoming: bool,
    /// 当前生效的放行规则（用于展示）。
    pub rules: Vec<FirewallRule>,
    /// 缺失的 `port/proto`（需要放行而不在规则里）。
    pub missing: Vec<String>,
}

impl FirewallStatus {
    /// 是否一切就绪（无缺失）。
    #[allow(dead_code)] // 前端经 missing 长度判断，Rust 侧暂无调用
    pub const fn ok(&self) -> bool {
        self.missing.is_empty()
    }
}

/// 解析 `ufw status verbose` 输出（纯函数，可单测）。
///
/// 输入形如：
/// ```text
/// Status: active
/// Logging: on (low)
/// Default: deny (incoming), allow (outgoing), disabled (routed)
/// New profiles: skip
///
/// To                         Action      From
/// --                         ------      ----
/// 22/tcp                     ALLOW       Anywhere
/// 18777/tcp                  ALLOW       192.168.11.0/24
/// ```
#[allow(dead_code)]
pub fn parse_ufw_verbose(text: &str) -> FirewallStatus {
    let mut ufw_active = false;
    let mut default_deny_incoming = false;
    let mut rules: Vec<FirewallRule> = Vec::new();
    // 表格区：遇到 `ALLOW` 且首列像 `port/proto` 的行才收（跳过 `To/Action/From` 表头与 `--` 分隔）
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("Status:") {
            ufw_active = line.contains("active");
        } else if line.starts_with("Default:") {
            default_deny_incoming = line.contains("deny (incoming)");
        } else if line.contains("ALLOW") && line.split_whitespace().count() >= 3 {
            let mut it = line.split_whitespace();
            let port_proto = it.next().unwrap_or_default().to_string();
            let action = it.next().unwrap_or_default();
            let from = it.next().unwrap_or_default().to_string();
            if action == "ALLOW"
                && !port_proto.is_empty()
                && port_proto.contains('/')
                && !from.is_empty()
            {
                rules.push(FirewallRule { port_proto, from });
            }
        }
    }
    FirewallStatus {
        ufw_active,
        default_deny_incoming,
        rules,
        missing: Vec::new(),
    }
}

/// 计算缺失放行：`required` 为 `port/proto` 列表；`subnet` 为本机局域网子网。
///
/// * ufw 未启用 / 入站默认允许 → 无需任何规则（缺失为空）
/// * 已启用且默认拒绝 → 每个必需端口须有规则来源覆盖 `subnet` 或 `Anywhere`
#[allow(dead_code)]
pub fn missing_rules(
    required: &[&str],
    rules: &[FirewallRule],
    subnet: &str,
    ufw_active: bool,
    default_deny_incoming: bool,
) -> Vec<String> {
    if !ufw_active || !default_deny_incoming {
        return Vec::new();
    }
    required
        .iter()
        .filter(|need| {
            !rules.iter().any(|r| {
                r.port_proto == **need
                    && (r.from == "Anywhere"
                        || r.from == subnet
                        || r.from == format!("{subnet} (v6)"))
            })
        })
        .map(std::string::ToString::to_string)
        .collect()
}

/// 本机局域网子网（CIDR /24）：取首个非回环 IPv4 → 192.168.x.0/24。
/// 无 IPv4 局域网地址时返回 `None`（无法生成来源限定规则）。
#[allow(dead_code)]
pub fn lan_subnet(ips: &[IpAddr]) -> Option<String> {
    ips.iter().find_map(|ip| match ip {
        IpAddr::V4(v4) if !v4.is_loopback() => {
            let octets = v4.octets();
            Some(format!("{}.{}.{}.0/24", octets[0], octets[1], octets[2]))
        }
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
Status: active
Logging: on (low)
Default: deny (incoming), allow (outgoing), disabled (routed)
New profiles: skip

To                         Action      From
--                         ------      ----
22/tcp                     ALLOW       Anywhere
18777/tcp                  ALLOW       192.168.11.0/24
33462/udp                  ALLOW       Anywhere
";

    #[test]
    fn parses_verbose_status() {
        let s = parse_ufw_verbose(SAMPLE);
        assert!(s.ufw_active);
        assert!(s.default_deny_incoming);
        assert_eq!(s.rules.len(), 3);
        assert_eq!(s.rules[0].port_proto, "22/tcp");
        assert_eq!(s.rules[1].from, "192.168.11.0/24");
    }

    #[test]
    fn missing_detects_uncovered_ports() {
        let s = parse_ufw_verbose(SAMPLE);
        // 本应放行的端口：真源在库层常量（防火墙规则不得自持端口号，
        // docs/layering-architecture.md：端口真源统一在库层）
        let required: Vec<String> = vec![
            format!("{}/tcp", stross_kernel::relay::DEFAULT_PORT),
            format!("{}/tcp", stross_kernel::DEFAULT_NEGOTIATOR_PORT),
            format!("{}/udp", stross_kernel::DEFAULT_SRT_PORT),
            format!("{}/udp", stross_kernel::DEFAULT_QUIC_PORT),
        ];
        let required_refs: Vec<&str> = required.iter().map(std::string::String::as_str).collect();
        let missing = missing_rules(
            &required_refs,
            &s.rules,
            "192.168.11.0/24",
            s.ufw_active,
            s.default_deny_incoming,
        );
        // 协商 TCP 与 QUIC UDP 缺失（SAMPLE 只放行了 18777/tcp、22/tcp、33462/udp）
        assert_eq!(
            missing,
            vec![
                format!("{}/tcp", stross_kernel::DEFAULT_NEGOTIATOR_PORT),
                format!("{}/udp", stross_kernel::DEFAULT_QUIC_PORT),
            ]
        );
    }

    #[test]
    fn no_rules_needed_when_inactive_or_allow_all() {
        let s = parse_ufw_verbose("");
        assert!(!s.ufw_active);
        assert!(missing_rules(&["18777/tcp"], &[], "", false, false).is_empty());
        // 默认允许入站：即使 ufw 启用也不需要规则
        let allow_all = parse_ufw_verbose("Status: active\nDefault: allow (incoming)");
        assert!(
            missing_rules(
                &["18777/tcp"],
                &[],
                "",
                true,
                allow_all.default_deny_incoming
            )
            .is_empty()
        );
    }

    #[test]
    fn lan_subnet_from_ipv4() {
        let ips = vec![
            "127.0.0.1".parse().unwrap(),
            "192.168.11.61".parse().unwrap(),
        ];
        assert_eq!(lan_subnet(&ips).as_deref(), Some("192.168.11.0/24"));
    }

    #[test]
    fn lan_subnet_none_without_lan_ipv4() {
        assert!(lan_subnet(&["127.0.0.1".parse().unwrap()]).is_none());
    }
}
