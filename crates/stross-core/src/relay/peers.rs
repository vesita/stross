//! 局域网设备发现缓存（feature `discovery`）。
//!
//! 中继周期 mDNS 浏览局域网内其它 Stross 中继，映射为 [`PeerInfo`] 存入
//! [`super::RelayState`]，供 `GET /api/peers` 展示；也可手动注册
//! （[`super::RelayState::insert_peer`]，调试 / 测试 / 跨网段补充）。

#[cfg(feature = "discovery")]
use std::collections::HashMap;
#[cfg(feature = "discovery")]
use std::net::IpAddr;
#[cfg(feature = "discovery")]
use std::time::Duration;

use serde::Serialize;
#[cfg(feature = "discovery")]
use stross_proto::message::DiscoveryInfo;
use stross_proto::message::{RoleId, TransportId};
#[cfg(feature = "discovery")]
use tokio::sync::watch;

/// 一个局域网内可发现的 Stross 中继（设备）。
///
/// 由 mDNS 浏览结果构造（feature `discovery` 的周期任务维护），
/// 也可手动注册（[`RelayState::insert_peer`]，调试 / 测试 / 跨网段补充）。
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerInfo {
    /// 唯一标识（`ip:port`）。
    pub id: String,
    /// 设备名（TXT `name`；缺失时回退为 `ip:port`）。
    pub name: String,
    pub ip: String,
    pub port: u16,
    /// 角色（能力引导 `roles`；枚举，序列化与字符串时代一致）。
    pub roles: Vec<RoleId>,
    /// 支持的传输（TXT `transports`，解析为枚举）。
    pub transports: Vec<TransportId>,
    /// 中继入口地址（`http://ip:port/`，数据面端点在 `ws://ip:port/ws/*`）。
    pub url: String,
}

/// 周期浏览局域网内其它 Stross 中继，更新 [`RelayState`] 的设备缓存。
///
/// 每 `BROWSE_INTERVAL` 浏览一次（每次 [`crate::discovery::Discovery::browse`]
/// 内置超时）；被浏览到的本机广播实例（本机 IP + 本机端口）会被剔除。
#[cfg(feature = "discovery")]
pub(super) fn spawn_peer_refresh(
    state: super::RelayState,
    self_port: u16,
    mut shutdown: watch::Receiver<bool>,
) {
    const BROWSE_INTERVAL: Duration = Duration::from_secs(15);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(BROWSE_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    match crate::discovery::Discovery::browse(Duration::from_secs(2)).await {
                        Ok(found) => {
                            let self_ips = crate::net::local_ips();
                            let peers = filter_self(found, self_port, &self_ips);
                            state.set_peers(peers);
                        }
                        Err(e) => tracing::warn!("局域网设备浏览失败: {e}"),
                    }
                }
                _ = shutdown.changed() => break,
            }
        }
        tracing::debug!("设备发现缓存已停止");
    });
}

/// 从 mDNS 浏览结果剔除本机中继（自己广播的实例），并映射为设备表。
#[cfg(feature = "discovery")]
fn filter_self(
    found: Vec<crate::discovery::Discovered>,
    self_port: u16,
    self_ips: &[IpAddr],
) -> HashMap<String, PeerInfo> {
    let mut out = HashMap::new();
    for d in found {
        // 本机广播的实例：本机 IP + 本机中继端口
        if d.port == self_port && self_ips.contains(&d.ip) {
            continue;
        }
        out.insert(format!("{}:{}", d.ip, d.port), peer_from_discovered(d));
    }
    out
}

/// 把一条 mDNS 浏览记录映射为 [`PeerInfo`]（能力引导缺失时回退默认值）。
#[cfg(feature = "discovery")]
fn peer_from_discovered(d: crate::discovery::Discovered) -> PeerInfo {
    // 单 key JSON 解码（F1.2 / 1d）；旧设备 / 缺失时回退默认
    let info = DiscoveryInfo::from_txt(&d.txt);
    PeerInfo {
        id: format!("{}:{}", d.ip, d.port),
        name: info
            .as_ref()
            .map(|i| i.name.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("{}:{}", d.ip, d.port)),
        ip: d.ip.to_string(),
        port: d.port,
        roles: info.as_ref().map(|i| i.roles.clone()).unwrap_or_default(),
        transports: info
            .as_ref()
            .map(|i| i.transports.clone())
            .unwrap_or_default(),
        url: format!("http://{}:{}/", d.ip, d.port),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::RelayState;
    use std::collections::HashMap;
    use stross_proto::message::CodecId;

    #[cfg(feature = "discovery")]
    fn discovered(ip: &str, port: u16, txt: Vec<(String, String)>) -> crate::discovery::Discovered {
        crate::discovery::Discovered {
            instance: format!("relay-{port}._stross._tcp.local."),
            ip: ip.parse().expect("测试 IP"),
            port,
            txt,
        }
    }

    #[test]
    fn peers_cache_insert_list_sort() {
        let state = RelayState::default();
        state.insert_peer(PeerInfo {
            id: "10.0.0.2:9000".into(),
            name: "Beta".into(),
            ip: "10.0.0.2".into(),
            port: 9000,
            roles: vec![RoleId::Relay],
            transports: vec![],
            url: "http://10.0.0.2:9000/".into(),
        });
        state.insert_peer(PeerInfo {
            id: "10.0.0.1:8777".into(),
            name: "Alpha".into(),
            ip: "10.0.0.1".into(),
            port: 8777,
            roles: vec![],
            transports: vec![],
            url: "http://10.0.0.1:8777/".into(),
        });
        let peers = state.peers();
        assert_eq!(peers.len(), 2);
        // 按名称排序：Alpha 在前
        assert_eq!(peers[0].name, "Alpha");
        assert_eq!(peers[1].name, "Beta");
    }

    #[test]
    fn peers_cache_replace_all() {
        let state = RelayState::default();
        let mut map = HashMap::new();
        map.insert(
            "10.0.0.1:8777".into(),
            PeerInfo {
                id: "10.0.0.1:8777".into(),
                name: "A".into(),
                ip: "10.0.0.1".into(),
                port: 8777,
                roles: vec![],
                transports: vec![],
                url: "http://10.0.0.1:8777/".into(),
            },
        );
        state.set_peers(map);
        assert_eq!(state.peers().len(), 1);
        // 整体替换：旧的被清空
        state.set_peers(HashMap::new());
        assert!(state.peers().is_empty());
    }

    #[cfg(feature = "discovery")]
    #[test]
    fn peer_from_discovered_parses_txt() {
        let info = DiscoveryInfo {
            v: DiscoveryInfo::VERSION,
            name: "客厅电脑".into(),
            roles: vec![RoleId::Sender, RoleId::Viewer, RoleId::Relay],
            media: vec![],
            transports: vec![
                TransportId::Ws,
                TransportId::WebRtc,
                TransportId::Srt,
                TransportId::Quic,
            ],
            codecs: vec![CodecId::H264, CodecId::Aac],
        };
        let p = peer_from_discovered(discovered("192.168.1.9", 8777, info.to_txt()));
        assert_eq!(p.name, "客厅电脑");
        assert_eq!(p.ip, "192.168.1.9");
        assert_eq!(p.port, 8777);
        assert_eq!(p.roles, vec![RoleId::Sender, RoleId::Viewer, RoleId::Relay]);
        assert_eq!(
            p.transports,
            vec![
                TransportId::Ws,
                TransportId::WebRtc,
                TransportId::Srt,
                TransportId::Quic
            ]
        );
        assert_eq!(p.url, "http://192.168.1.9:8777/");
    }

    #[cfg(feature = "discovery")]
    #[test]
    fn peer_from_discovered_falls_back_without_txt() {
        let p = peer_from_discovered(discovered("10.0.0.7", 9001, vec![]));
        assert_eq!(p.name, "10.0.0.7:9001");
        assert!(p.roles.is_empty());
        assert!(p.transports.is_empty());
    }

    #[cfg(feature = "discovery")]
    #[test]
    fn filter_self_drops_own_broadcast() {
        let self_ip: IpAddr = "192.168.1.5".parse().unwrap();
        let found = vec![
            discovered("192.168.1.5", 8777, vec![]), // 自己（本机 IP + 本机端口）
            discovered("192.168.1.5", 9000, vec![]), // 本机另一实例（不同端口，保留）
            discovered("192.168.1.8", 8777, vec![]), // 其它设备同端口（保留）
            discovered("192.168.1.9", 9001, vec![]), // 其它设备（保留）
        ];
        let peers = filter_self(found, 8777, &[self_ip]);
        assert_eq!(peers.len(), 3);
        assert!(peers.contains_key("192.168.1.5:9000"));
        assert!(peers.contains_key("192.168.1.8:8777"));
        assert!(peers.contains_key("192.168.1.9:9001"));
    }
}
