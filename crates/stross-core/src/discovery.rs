//! mDNS 服务发现（可选 feature `discovery`）。
//!
//! 中继/推流端用 `_stross._tcp` 广播自己，局域网内其它设备可以发现观看地址。
//! 借鉴 [mdns-sd](https://crates.io/crates/mdns-sd) 的用法。

use std::net::IpAddr;
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

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
    /// 以 `instance` 名义广播服务。
    pub fn start(
        instance: &str,
        ip: IpAddr,
        port: u16,
        txt: &[(&str, &str)],
    ) -> anyhow::Result<Self> {
        let daemon = ServiceDaemon::new()?;
        let hostname = hostname::get().unwrap_or_else(|_| "stross".into());
        let host = format!("{}.local.", hostname.to_string_lossy());
        let mut info = ServiceInfo::new(SERVICE_TYPE, instance, &host, ip, port, txt)
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
                    let ip = info.get_addresses().iter().next().copied();
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
