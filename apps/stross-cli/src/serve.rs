//! `stross serve`：Stross 常驻实例（内核 + 受控中继 + 采集后端 + 控制面）。
//!
//! 模型（D7）：Stross 正常运行，CLI 通过 `stross ctrl` 接入控制面异步控制。
//! 控制面**仅绑定回环**（信任边界 = 本机用户，LAN 零暴露）。

use std::sync::Arc;

use clap::Args;
use stross_app::{CtrlServer, Platform, StrossApp, bootstrap};
use stross_media::capture::FfmpegBackend;

#[derive(Args, Debug)]
pub struct ServeArgs {
    /// 中继端口（0 = 随机；被占用时回退随机）
    #[arg(short, long, default_value_t = 18777)]
    pub port: u16,
    /// 控制面端口（0 = 随机；仅回环绑定，D7）
    #[arg(long, default_value_t = 18778)]
    pub ctrl_port: u16,
    /// SRT 传输端口（0 = 随机；固定便于防火墙放行）
    #[arg(long, default_value_t = stross_app::DEFAULT_SRT_PORT)]
    pub srt_port: u16,
    /// QUIC 传输端口（0 = 随机；固定便于防火墙放行）
    #[arg(long, default_value_t = stross_app::DEFAULT_QUIC_PORT)]
    pub quic_port: u16,
}

pub async fn run(args: ServeArgs) -> anyhow::Result<()> {
    let app = Arc::new(StrossApp::new(Platform::Desktop));
    // 桌面采集后端（ffmpeg），供 ctrl start-stream 使用
    app.set_backend(Arc::new(FfmpegBackend::new()));
    // 引导层（docs/endpoint-model.md §0）：身份注入 → 锚定受控中继并广播
    // mDNS L1 摘要（节点 → 设备清单）→ 目录/订阅握手端点（18779）。
    // 与 GUI 桌面共用同一套启动原语。
    let base = std::env::var("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|h| std::path::Path::new(&h).join(".local/share/stross"))
                .unwrap_or_else(|_| std::path::PathBuf::from("stross-data"))
        });
    bootstrap::ensure_identity(&app, &base);
    let bootstrap_handle = bootstrap::start(
        app.clone(),
        Arc::new(stross_app::CliUi),
        &base,
        args.port,
        args.srt_port,
        args.quic_port,
    )
    .await?;
    tracing::info!(
        "中继已启动: ws://127.0.0.1:{}/ws/push",
        bootstrap_handle.relay.port
    );
    tracing::info!(
        "凭证协商: 手机端「共享麦克风到 TA」自动接入，审批: stross ctrl negotiator-list"
    );

    let ctrl = CtrlServer::start(
        app.clone(),
        args.ctrl_port,
        Some(bootstrap_handle.negotiator()),
    )
    .await?;
    tracing::info!(
        "接入控制: stross ctrl --connect ws://127.0.0.1:{}/ws/ctrl",
        ctrl.port
    );

    tokio::signal::ctrl_c().await?;
    tracing::info!("正在停止…");
    ctrl.stop().await;
    app.stop_stream().await.ok();
    Ok(())
}
