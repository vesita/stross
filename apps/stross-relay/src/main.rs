//! stross-relay —— 独立的局域网串流中继。
//!
//! 只承载数据面（/ws/push、/ws/watch）与 REST 端点（/api/*）；无内置观看页，
//! 接收端用原生播放（GUI「接收」页 / stross 命令），见 docs/requirements.md D1。
//!
//! 启动样板（中继启动 → 打印入口 → mDNS 广播 → 等待 Ctrl+C）统一在
//! `RelayServer::run_standalone`（stross-core），本二进制只负责解析 CLI 参数。
//!
//! ```text
//! 用法:
//!   stross-relay                       # 默认 0.0.0.0:8777
//!   stross-relay -p 9000               # 指定端口
//!   stross-relay -p 0                  # 随机端口
//! ```

use clap::Parser;
use stross_kernel::relay::{DEFAULT_PORT, RelayServer};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "stross-relay", version, about = "Stross 局域网串流中继")]
struct Args {
    /// 监听端口（0 = 随机）
    #[arg(short, long, default_value_t = DEFAULT_PORT)]
    port: u16,

    /// 关闭 mDNS 广播（默认广播自己，便于局域网内设备自动发现）
    #[arg(long)]
    no_advertise: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    // mDNS 广播主机名：壳层平台适配负责取本机名（core 零 OS 调用）
    let hostname =
        hostname::get().map_or_else(|_| "stross".into(), |h| h.to_string_lossy().to_string());
    RelayServer::run_standalone(args.port, !args.no_advertise, "relay", &hostname).await
}
