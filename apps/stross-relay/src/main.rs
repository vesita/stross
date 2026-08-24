//! stross-relay —— 独立的局域网串流中继。
//!
//! 内嵌观看端页面，局域网内任意设备（Linux / Windows / Android 浏览器）打开
//! `http://<本机IP>:<端口>` 即可观看。
//!
//! ```text
//! 用法:
//!   stross-relay                       # 默认 0.0.0.0:8777
//!   stross-relay -p 9000               # 指定端口
//!   stross-relay -p 0                  # 随机端口
//! ```



use clap::Parser;
use stross_core::net::local_ips;
use stross_core::relay::{RelayServer, DEFAULT_PORT};
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
    let url = |ip: String| format!("http://{ip}:{}/", handle.port);
    println!("\n  📡 Stross 中继已启动\n");
    for ip in &ips {
        println!("     观看地址: {}", url(ip.to_string()));
    }
    if ips.is_empty() {
        println!("     观看地址: http://127.0.0.1:{}/", handle.port);
    }
    println!("\n     推流地址: ws://<中继IP>:{}/ws/push", handle.port);
    println!("     流列表API: /api/streams");
    println!("\n  Ctrl+C 退出\n");

    #[cfg(feature = "discovery")]
    if !args.no_advertise {
        let mut discovery = stross_core::discovery::Discovery::start(
            &format!("relay-{}", handle.port),
            local_ips().first().copied().unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            handle.port,
            &[
                ("kind", "relay"),
                ("name", "Stross 中继"),
                ("roles", "sender,viewer,relay"),
                ("transports", "ws,webrtc,srt,quic"),
                ("codecs", "h264,aac"),
            ],
        )?;
        println!("  mDNS 广播中…");
        tokio::signal::ctrl_c().await?;
        discovery.stop();
    } else {
        println!("  mDNS 广播已关闭");
        tokio::signal::ctrl_c().await?;
    }

    #[cfg(not(feature = "discovery"))]
    tokio::signal::ctrl_c().await?;

    handle.stop().await;
    Ok(())
}
