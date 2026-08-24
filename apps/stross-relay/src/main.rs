//! stross-relay —— 独立的局域网串流中继。
//!
//! 只承载数据面（/ws/push、/ws/watch）与 REST 端点（/api/*）；无内置观看页，
//! 接收端用原生播放（GUI「接收」页 / stross 命令），见 docs/requirements.md D1。
//!
//! ```text
//! 用法:
//!   stross-relay                       # 默认 0.0.0.0:8777
//!   stross-relay -p 9000               # 指定端口
//!   stross-relay -p 0                  # 随机端口
//! ```

use clap::Parser;
use stross_core::net::local_ips;
use stross_core::relay::{DEFAULT_PORT, RelayServer};
use stross_proto::message::{CodecId, DiscoveryInfo, RoleId, TransportId};
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
    let handle = RelayServer::start(args.port).await?;

    let ips = local_ips();
    tracing::info!("📡 Stross 中继已启动");
    if ips.is_empty() {
        tracing::info!("中继入口: http://127.0.0.1:{}/", handle.port);
    }
    for ip in &ips {
        tracing::info!("中继入口: http://{ip}:{}/", handle.port);
    }
    tracing::info!("推流地址: ws://<中继IP>:{}/ws/push", handle.port);
    tracing::info!("流列表API: /api/streams");
    tracing::info!("Ctrl+C 退出");

    #[cfg(feature = "discovery")]
    if !args.no_advertise {
        let mut discovery = stross_core::discovery::Discovery::start(
            &format!("relay-{}", handle.port),
            local_ips()
                .first()
                .copied()
                .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            handle.port,
            &DiscoveryInfo {
                v: DiscoveryInfo::VERSION,
                name: "Stross 中继".into(),
                roles: vec![RoleId::Relay, RoleId::Sender, RoleId::Viewer],
                media: vec![],
                transports: vec![
                    TransportId::Ws,
                    TransportId::WebRtc,
                    TransportId::Srt,
                    TransportId::Quic,
                ],
                codecs: vec![CodecId::H264, CodecId::Aac],
            },
        )?;
        tracing::info!("mDNS 广播中…");
        tokio::signal::ctrl_c().await?;
        discovery.stop();
    } else {
        tracing::info!("mDNS 广播已关闭");
        tokio::signal::ctrl_c().await?;
    }

    #[cfg(not(feature = "discovery"))]
    tokio::signal::ctrl_c().await?;

    handle.stop().await;
    Ok(())
}
