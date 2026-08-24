//! mDNS 服务发现（可选 feature `discovery`）。
//!
//! 中继/推流端用 `_stross._tcp` 广播自己，局域网内其它设备可以发现观看地址。
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
    pub fn start(
        instance: &str,
        ip: IpAddr,
        port: u16,
        info: &DiscoveryInfo,
    ) -> anyhow::Result<Self> {
        let daemon = ServiceDaemon::new()?;
        let hostname = hostname::get().unwrap_or_else(|_| "stross".into());
        let host = format!("{}.local.", hostname.to_string_lossy());
        // mdns-sd 0.21：TXT 属性走 IntoTxtProperties（HashMap<String, String>）；
        // 能力描述由 DiscoveryInfo 单 key JSON 编码（新增字段零维护）
        let props: std::collections::HashMap<String, String> = info.to_txt().into_iter().collect();
        let mut info = ServiceInfo::new(SERVICE_TYPE, instance, &host, ip, port, props)
            .map_err(|e| anyhow::anyhow!("ServiceInfo: {e}"))?;
        info = info.enable_addr_auto();
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
                    // mdns-sd 0.21：地址为 HashSet<ScopedIp>（带接口信息）
                    let ip = info.get_addresses().iter().next().map(|s| s.to_ip_addr());
                    if let Some(ip) = ip {
                        out.push(Discovered {
                            instance: info.get_fullname().to_string(),
                            ip,
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
