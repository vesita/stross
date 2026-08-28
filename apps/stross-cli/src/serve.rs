//! `stross serve`：Stross 常驻实例（内核 + 受控中继 + 采集后端 + 控制面）。
//!
//! 模型（D7）：Stross 正常运行，CLI 通过 `stross ctrl` 接入控制面异步控制。
//! 控制面**仅绑定回环**（信任边界 = 本机用户，LAN 零暴露）。

use std::path::PathBuf;
use std::sync::Arc;

use clap::Args;
use stross_bridge::{device_name_or, seed_platform_devices};
use stross_kernel::{CtrlServer, Kernel, Platform, bootstrap};
use stross_media::capture::FfmpegBackend;

#[derive(Args, Debug)]
pub struct ServeArgs {
    /// 中继端口（0 = 随机；被占用时回退随机）。默认 = 协议约定固定端口
    #[arg(short, long, default_value_t = stross_kernel::relay::DEFAULT_PORT)]
    pub port: u16,
    /// 控制面端口（0 = 随机；仅回环绑定，D7）
    #[arg(long, default_value_t = stross_kernel::DEFAULT_CTRL_PORT)]
    pub ctrl_port: u16,
    /// SRT 传输端口（0 = 随机；固定便于防火墙放行）
    #[arg(long, default_value_t = stross_kernel::DEFAULT_SRT_PORT)]
    pub srt_port: u16,
    /// QUIC 传输端口（0 = 随机；固定便于防火墙放行）
    #[arg(long, default_value_t = stross_kernel::DEFAULT_QUIC_PORT)]
    pub quic_port: u16,
    /// 目录/订阅握手端点端口（0 = 18779；本地双端测试用自定义端口避免冲突）
    #[arg(long, default_value_t = stross_kernel::DEFAULT_NEGOTIATOR_PORT)]
    pub negotiator_port: u16,
    /// 身份/信任清单数据目录（默认 ~/.local/share/stross；本地双端测试需
    /// 两个节点用不同目录 → 不同 device_id）
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
}

/// 数据目录解析（identity.json / trusted_devices.json 所在）：
/// 单一真源收敛在 `stross_bridge::data_dir`（docs/layering-architecture.md）。
fn base_dir(data_dir: Option<PathBuf>) -> PathBuf {
    stross_bridge::data_dir(data_dir)
}

pub async fn run(args: ServeArgs) -> anyhow::Result<()> {
    let app = Arc::new(Kernel::new(Platform::Desktop));
    // 桌面采集后端（ffmpeg），供 ctrl start-stream 使用
    app.set_backend(Arc::new(FfmpegBackend::new()));
    // 平台设备清单（桥接层单一来源：桌面 = 屏幕/麦克风/系统声音）
    seed_platform_devices(&app);
    // 端点订阅驱动：订阅达成自动开推（文件泵 / 媒体推流），docs/endpoint-model.md §5
    // —— 已收敛为 bootstrap::start 的默认行为（幂等），此处无需再手动接线。
    // 引导层（docs/endpoint-model.md §0）：身份注入 → 锚定受控中继并广播
    // mDNS L1 摘要（节点 → 设备清单）→ 目录/订阅握手端点；
    // 与 GUI 桌面共用同一套启动原语（主机名经桥接层注入，内核零 OS 调用）。
    let base = base_dir(args.data_dir.clone());
    bootstrap::ensure_identity(&app, &base, &device_name_or("Stross 设备"));
    let bootstrap_handle = bootstrap::start(
        app.clone(),
        Arc::new(stross_kernel::CliUi),
        &base,
        args.port,
        args.srt_port,
        args.quic_port,
        args.negotiator_port,
        &device_name_or("Stross 设备"),
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
