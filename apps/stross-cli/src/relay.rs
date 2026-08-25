//! `stross relay`：启动局域网中继（等同独立 `stross-relay`）。

use clap::Args;
use stross_core::relay::{DEFAULT_PORT, RelayServer};
use stross_proto::message::{CodecId, DiscoveryInfo, RoleId, TransportId};

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
    let handle = RelayServer::start(args.port).await?;
    let ips = stross_core::net::local_ips();
    tracing::info!("📡 Stross 中继已启动");
    if ips.is_empty() {
        tracing::info!("中继入口: http://127.0.0.1:{}/", handle.port);
    }
    for ip in &ips {
        tracing::info!("中继入口: http://{ip}:{}/", handle.port);
    }
    tracing::info!("推流地址: ws://<中继IP>:{}/ws/push", handle.port);
    if let Some(p) = handle.srt_port {
        tracing::info!("SRT 推流地址: srt://<中继IP>:{p}（视频默认，UDP 自适应）");
    }
    if let Some(p) = handle.quic_port {
        tracing::info!("QUIC 推流地址: quic://<中继IP>:{p}（音频默认，UDP 无损）");
    }
    tracing::info!("流列表API: /api/streams");
    tracing::info!("中继信息API: /api/info（srtPort/quicPort 自动发现）");
    tracing::info!("Ctrl+C 退出");

    #[cfg(feature = "discovery")]
    if !args.no_advertise {
        let mut discovery = stross_core::discovery::Discovery::start(
            &format!("relay-{}", handle.port),
            &ips,
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
