//! mDNS 服务发现（可选 feature `discovery`）。
//!
//! 中继/推流端用 `_stross._tcp` 广播自己，局域网内其它设备可以发现中继入口。
//! 借鉴 [mdns-sd](https://crates.io/crates/mdns-sd) 的用法。
//!
//! 能力引导（F1.2）：TXT 单 key（`stross`）承载整个 [`DiscoveryInfo`]（JSON），
//! 见 [`stross_proto::message::DiscoveryInfo`]——注册侧传结构体，浏览侧解码结构体。

use std::net::{IpAddr, Ipv4Addr};
use std::sync::OnceLock;
use std::time::Duration;

use mdns::{ServiceDaemon, ServiceEvent, ServiceInfo};
use stross_proto::message::DiscoveryInfo;

/// mDNS 服务类型。
pub const SERVICE_TYPE: &str = "_stross._tcp.local.";

/// browse 默认超时。
///
/// 根因（mdns-sd 0.21 的 resolve 二次链路）已在本地 fork（crates/mdns）
/// 内部根治：resolve 查询指数退避重试（0.5/1/2/4 s，覆盖整个 browse
/// 窗口）+ stop_browse 保留缓存（跨 browse 秒回，SRV/A 由 120s TTL 自愈），
/// ANY 查询响应同时携带 A/AAAA additionals（单往返完成解析）。4 秒仍保留
/// 为浏览窗口：足够 3 次 resolve 尝试落网，同时兼顾首扫冷发现。
pub const BROWSE_TIMEOUT: Duration = Duration::from_secs(4);

/// 浏览聚合条目：实例名 →（可达地址集, 端口, TXT 键值）。
type BrowseAgg = std::collections::HashMap<
    String,
    (Vec<IpAddr>, u16, std::collections::HashMap<String, String>),
>;

/// 一个被发现的服务实例。
#[derive(Debug, Clone)]
pub struct Discovered {
    pub instance: String,
    pub ip: IpAddr,
    pub port: u16,
    pub txt: Vec<(String, String)>,
}

/// 进程级共享的 mDNS daemon。
///
/// **关键**：register（广播本机）与 browse（扫描对端）必须共用**同一个**
/// `ServiceDaemon`。若各自 `new()`（各起一个 bind 5353 的 socket），
/// mdns-sd 默认 `SO_REUSEPORT` 会导致跨设备组播响应被另一个 socket 分摊抢走，
/// browse 只能收到本机回环的自己广播、收不到对端（真机双向失效的根因）。
fn daemon() -> &'static ServiceDaemon {
    static DAEMON: OnceLock<ServiceDaemon> = OnceLock::new();
    DAEMON.get_or_init(|| ServiceDaemon::new().expect("创建 mDNS daemon 失败"))
}

/// mDNS 广播句柄。
///
/// 保存注册时的完整参数，供 [`Discovery::redefine`] 保持同 fullname **覆盖更新**
/// （TXT 摘要变化时重注册，避免先 unregister 再 register 的空窗）。
pub struct Discovery {
    /// 本句柄注册的服务全名（Drop 时反注册，不关闭全局 daemon）。
    fullname: Option<String>,
    /// 服务实例（redefine 保持同实例名 → 同 fullname → 覆盖更新）。
    instance: String,
    /// 广告主机名（`{hostname}.local.`）。
    host: String,
    /// 广播地址集（来自调用方 `local_ips()`，redefine 复用）。
    addrs: Vec<IpAddr>,
    /// 服务端口。
    port: u16,
}

impl Discovery {
    /// 以 `instance` 名义广播服务（能力描述见 [`DiscoveryInfo`]）。
    ///
    /// `ips` 为要广播的本机局域网地址：**多网卡时全部传入**（一次注册
    /// 即携带全部 A/AAAA 记录，各网卡网段都能扫描到本机）；空列表回退
    /// 回环地址（至少保证本机可发现，行为与单 IP 时代一致）。
    ///
    /// `hostname` 为 mDNS 广告主机名（`{hostname}.local.`，调用方负责取本机
    /// 名——core 零 OS 服务调用，见 docs/layering-architecture.md 平台无关红线；
    /// 取不到时调用方可传 "stross" 兜底）。
    pub fn start(
        instance: &str,
        ips: &[IpAddr],
        port: u16,
        info: &DiscoveryInfo,
        hostname: &str,
    ) -> anyhow::Result<Self> {
        let host = format!("{hostname}.local.");
        let addrs = broadcast_addrs(ips);
        // 能力描述由 DiscoveryInfo 单 key JSON 编码（新增字段零维护）
        let props: std::collections::HashMap<String, String> = info.to_txt().into_iter().collect();
        // 多网卡广播：ServiceInfo::new 支持 AsIpAddrs（&[IpAddr]），
        // 一次注册携带全部地址记录
        let mut info =
            ServiceInfo::new(SERVICE_TYPE, instance, &host, addrs.as_slice(), port, props)
                .map_err(|e| anyhow::anyhow!("ServiceInfo: {e}"))?;
        // 显式地址为准：**不启用 enable_addr_auto()**。
        // 原因（Android 真机实测）：mdns 的 `if_addrs::get_if_addrs()` 会枚举到
        // dummy0/ifb0/ifb1 等虚拟接口及其 fe80 地址，auto 覆盖后把真实 wlan0 IPv4
        // （如 192.168.11.60）挤掉，导致对端扫到手机只有不可达的 fe80 地址。
        // 调用方 `local_ips()` 已返回真实局域网 IPv4，显式传入即可；
        // 代价：WiFi 切换时需重新 start_relay 再注册（本应用每次锚定都会重走）。
        //
        // **跳过探测（requires_probe=false）**：`prepare_announce` 的 `probing_count`
        // 闸门在 SRV/TXT/A 任一记录 `is_probing_done` 为 false 时丢弃整条 QR=1 公告
        // （连 PTR 一起）。而 `DnsAddress::matches` 要求 interface_id（name+index）
        // 相等，Android 多网卡（wlan0/ifb0/ifb1/dummy0/ccmni0/ccmni1 均 `up && !p2p &&
        // !lo` 进 `my_intfs`，同一 hostname 每接口一份 A 探针）重建记录与 active 记录
        // 接口身份不匹配 → `is_probing_done` 恒 false → 无限重探测 → `probing_count`
        // 恒 > 0 → 永不发 QR=1 完整公告（真机实测：手机只广播 QR=0 PTR，对 SRV/ANY
        // 查询只回 PTR）。实例名由 deviceId 生成、本机唯一，跳过探测可接受。
        info.set_requires_probe(false);
        let fullname = info.get_fullname().to_string();
        daemon().register(info)?;
        Ok(Self {
            fullname: Some(fullname),
            instance: instance.to_string(),
            host,
            addrs,
            port,
        })
    }

    /// 重注册本服务，更新 TXT 能力摘要（端点通告 / 取消通告后调用）。
    ///
    /// **保持同 fullname**：mdns-sd 的 `my_services` 按全名（小写）作 key，
    /// 同 fullname 直接覆盖（`register_service` 里 `HashMap::insert`），并重新
    /// 广播公告——对端收到的是**同一服务**的最新 TXT，而非「注销 + 重启」。
    ///
    /// `DiscoveryInfo` 不同处的字段（如端点 `published`）经 `to_txt` 编码后
    /// 全部进 TXT，故本次仅重传新的能力描述即可。
    pub fn redefine(&mut self, info: &DiscoveryInfo) -> anyhow::Result<()> {
        let Some(fullname) = self.fullname.as_ref() else {
            return Ok(());
        };
        let props: std::collections::HashMap<String, String> = info.to_txt().into_iter().collect();
        let mut new_info = ServiceInfo::new(
            SERVICE_TYPE,
            &self.instance,
            &self.host,
            self.addrs.as_slice(),
            self.port,
            props,
        )
        .map_err(|e| anyhow::anyhow!("ServiceInfo: {e}"))?;
        // 与 `start` 一致：跳过探测，重注册即立即发 QR=1 完整公告（见 `start` 注释）。
        new_info.set_requires_probe(false);
        // register_service 用全名（小写）覆盖同 key——TXT 更新即生效。
        if fullname != new_info.get_fullname() {
            tracing::warn!(
                "mDNS 重注册 fullname 变化（{fullname} → {}）：对端会视为新服务",
                new_info.get_fullname()
            );
        }
        daemon().register(new_info)?;
        Ok(())
    }

    /// 在 `timeout` 内浏览局域网内的 Stross 服务。
    pub async fn browse(timeout: Duration) -> anyhow::Result<Vec<Discovered>> {
        // 与 GUI 已知可用配置对齐：接受 unsolicited 响应。否则对端周期公告
        // （非查询应答的 PTR/SRV）会被 handle_response 的 is_for_us 过滤丢弃，
        // 「只浏览不注册」的进程（CLI devices / 纯扫描）将收不到任何局域网
        // 设备（真机实测：手机广播到达本机但 browse 零结果）。
        daemon()
            .accept_unsolicited(true)
            .map_err(|e| anyhow::anyhow!("accept_unsolicited: {e}"))?;
        let receiver = daemon().browse(SERVICE_TYPE)?;
        let deadline = tokio::time::Instant::now() + timeout;
        // 按实例名聚合：mdns 增量 resolve 会多次触发 ServiceResolved（每次
        // 地址集合逐步补全 A/AAAA）。若对每个事件立即选址，调用方会拿到
        // 地址不全的首个事件（真机：手机首次事件仅 rndis0 的 10.159.157.104，
        // 二次才补 wlan0 的 192.168.2.6——旧逻辑据此选中不可达的虚拟接口）。
        // 这里把每次可达地址并入集合（去重），浏览结束时对**最全**集合选址；
        // ServiceRemoved 同步移除条目。
        let mut agg: BrowseAgg = std::collections::HashMap::new();
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
                    // 1. link-local（fe80::/10、169.254/16）无 scope 不可达，剔除；
                    // 2. 其余全收进聚合集合（IPv4/IPv6 都留，最终选址再挑）。
                    let fullname = info.get_fullname().to_string();
                    let entry = agg
                        .entry(fullname.clone())
                        .or_insert_with(|| (Vec::new(), info.get_port(), Default::default()));
                    entry.1 = info.get_port();
                    // TXT 以最新事件为准（能力描述只在注册时变化）
                    entry.2 = info
                        .get_properties()
                        .iter()
                        .map(|p| (p.key().to_string(), p.val_str().to_string()))
                        .collect();
                    for addr in info.get_addresses() {
                        let ip = addr.to_ip_addr();
                        let keep = match ip {
                            IpAddr::V4(v4) => !v4.is_link_local() && !v4.is_unspecified(),
                            IpAddr::V6(v6) => {
                                v6.segments()[0] & 0xffc0 != 0xfe80 && !v6.is_unspecified()
                            }
                        };
                        if keep && !entry.0.contains(&ip) {
                            entry.0.push(ip);
                        }
                    }
                    tracing::debug!(
                        "mDNS 解析到服务 {fullname}，聚合地址 {:?}",
                        entry
                            .0
                            .iter()
                            .map(std::string::ToString::to_string)
                            .collect::<Vec<_>>(),
                    );
                }
                Ok(ServiceEvent::ServiceRemoved(fullname, _)) => {
                    agg.remove(&fullname);
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        // 停止本次 browse（全局 daemon 持续存活，仅停止浏览，不 shutdown）
        let _ = daemon().stop_browse(SERVICE_TYPE);
        Ok(agg
            .into_iter()
            .filter_map(|(instance, (addrs, port, txt))| {
                let txt_vec: Vec<(String, String)> = txt.into_iter().collect();
                select_reachable_ip(&addrs).map(|ip| Discovered {
                    instance,
                    ip: *ip,
                    port,
                    txt: txt_vec,
                })
            })
            .collect())
    }

    /// 停止广播（反注册本句柄注册的服务；全局 daemon 持续存活）。
    pub fn stop(&mut self) {
        if let Some(fullname) = self.fullname.take() {
            let _ = daemon().unregister(&fullname);
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

/// 从解析到的候选地址里选**可拨号**的一个。
///
/// 对端多网卡（手机 wlan0/rndis0/vgate0、PC 多网卡 + TUN）会广播多个 IPv4，
/// mdns-sd 的地址集合无序（HashSet）——此前直接取第一个 IPv4，真机实测
/// 会随机挑中不可达的虚拟接口地址（PC 扫到手机 rndis0 的 10.159.157.104、
/// 手机扫到 PC USB 网卡的 192.168.10.111，均点不开卡片）。
///
/// 选址（与广播端 `broadcast_addrs` 对称的启发式），**纯函数**：
/// 1. 优先「与本机任一 IPv4 同 /24 网段」的地址——同网段才可能在同一
///    局域网内直连（无线电广播来的地址几乎总是同网段）；
/// 2. 其次任选一个 IPv4（比 IPv6 可靠：双栈 WiFi 上前缀常互不可达）；
/// 3. 最后任意地址（纯 IPv6 局域网后备）。
///
/// `self_ips` 显式传入（运行时 = `local_ips()`，测试 = 固定网段），
/// 选址结果与运行环境解耦（本机网卡变化不再影响单测确定性）。
fn select_reachable_ip_from<'a>(
    self_ips: &[IpAddr],
    reachable: &'a [IpAddr],
) -> Option<&'a IpAddr> {
    reachable
        .iter()
        .find(|i| {
            let IpAddr::V4(v4) = i else {
                return false;
            };
            self_ips.iter().any(|s| {
                let IpAddr::V4(sv4) = s else {
                    return false;
                };
                ipv4_same_subnet(*sv4, *v4)
            })
        })
        .or_else(|| reachable.iter().find(|i| i.is_ipv4()))
        .or_else(|| reachable.first())
}

/// 运行时入口：以本机实际局域网地址做同网段偏好。
fn select_reachable_ip(reachable: &[IpAddr]) -> Option<&IpAddr> {
    select_reachable_ip_from(&crate::net::local_ips(), reachable)
}

/// 两个 IPv4 是否同 /24 网段。
const fn ipv4_same_subnet(a: Ipv4Addr, b: Ipv4Addr) -> bool {
    let a = u32::from_be_bytes(a.octets());
    let b = u32::from_be_bytes(b.octets());
    a & 0xffff_ff00 == b & 0xffff_ff00
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

    #[test]
    fn same_subnet_matches_third_octet() {
        assert!(ipv4_same_subnet(
            "192.168.2.32".parse().unwrap(),
            "192.168.2.6".parse().unwrap(),
        ));
        assert!(!ipv4_same_subnet(
            "192.168.2.32".parse().unwrap(),
            "10.159.157.104".parse().unwrap(),
        ));
        assert!(!ipv4_same_subnet(
            "192.168.2.32".parse().unwrap(),
            "192.168.3.1".parse().unwrap(),
        ));
    }

    // 注：选址测试一律走纯函数 `select_reachable_ip_from`，显式注入本机网段，
    // 不再依赖跑测试机器的实时网卡（旧测试硬编码"本机在某网段"，网段迁移
    // 后必挂——实测复现于机器从 192.168.2.x 迁到 192.168.11.x）。
    fn self_ips(cidrs: &[&str]) -> Vec<IpAddr> {
        cidrs.iter().map(|s| s.parse().unwrap()).collect()
    }

    #[test]
    fn select_prefers_same_subnet_over_other_v4() {
        // 本机 192.168.2.x：候选含同网段 wlan0 与异网段 rndis0/vgate0
        // 时，必须选中 wlan0（此前随机 HashSet 顺序会挑错，真机 bug）
        let self_ips = self_ips(&["192.168.2.32"]);
        let candidates = vec![
            IpAddr::V4("10.159.157.104".parse().unwrap()), // rndis0
            IpAddr::V4("172.30.242.158".parse().unwrap()), // vgate0
            IpAddr::V4("192.168.2.6".parse().unwrap()),    // wlan0（同网段）
        ];
        assert_eq!(
            select_reachable_ip_from(&self_ips, &candidates),
            Some(&IpAddr::V4("192.168.2.6".parse().unwrap()))
        );
    }

    #[test]
    fn select_multihomed_prefers_first_subnet_match() {
        // 本机多网卡（WiFi + USB）：候选按出现顺序，首个与任一本地网段
        // 重合的地址胜出——启发式只保证"同网段优先"，具体哪个由候选序决定
        let self_ips = self_ips(&["192.168.2.32", "10.159.157.104"]);
        let candidates = vec![
            IpAddr::V4("192.168.2.6".parse().unwrap()),
            IpAddr::V4("10.159.157.104".parse().unwrap()),
        ];
        assert_eq!(
            select_reachable_ip_from(&self_ips, &candidates),
            Some(&IpAddr::V4("192.168.2.6".parse().unwrap()))
        );
    }

    #[test]
    fn select_falls_back_to_first_v4_when_no_same_subnet() {
        // 本机网段与全部候选都不重合 → 回退第一个 IPv4
        let self_ips = self_ips(&["10.0.0.99"]);
        let candidates = vec![
            IpAddr::V4("10.0.0.5".parse().unwrap()),
            IpAddr::V6("fe80::1".parse().unwrap()),
        ];
        assert_eq!(
            select_reachable_ip_from(&self_ips, &candidates),
            Some(&IpAddr::V4("10.0.0.5".parse().unwrap()))
        );
    }

    #[test]
    fn select_accepts_v6_only_fallback() {
        // 只有 IPv6 候选（纯 IPv6 局域网），即使本机是 IPv4-only 也兜底返回
        let self_ips = self_ips(&["192.168.1.1"]);
        let candidates = vec![IpAddr::V6("fd00::1".parse().unwrap())];
        assert_eq!(
            select_reachable_ip_from(&self_ips, &candidates),
            Some(&IpAddr::V6("fd00::1".parse().unwrap()))
        );
    }
}
