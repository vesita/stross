//! `stross relay`：启动局域网中继（等同独立 `stross-relay`）。
//!
//! 分层（docs/framework-v3.md）：启动样板统一在
//! `RelayServer::run_standalone`（stross-core），本命令与 `stross-relay`
//! 二进制同源，只解析参数 + 取主机名（平台适配）。

use clap::Args;
use stross_kernel::relay::{DEFAULT_PORT, RelayServer};

#[derive(Args, Debug)]
pub struct RelayArgs {
    /// 监听端口（0 = 随机）
    #[arg(short, long, default_value_t = DEFAULT_PORT)]
    pub port: u16,
    /// 关闭 mDNS 广播（默认广播自己，便于局域网内设备自动发现）
    #[arg(long)]
    pub no_advertise: bool,
}

pub async fn run(args: RelayArgs) -> anyhow::Result<()> {
    let hostname =
        hostname::get().map_or_else(|_| "stross".into(), |h| h.to_string_lossy().to_string());
    RelayServer::run_standalone(args.port, !args.no_advertise, "relay", &hostname).await
}
