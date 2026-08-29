//! `stross endpoint`：端点框架（docs/endpoint-model.md）的**订阅方交互面**。
//!
//! 分层（docs/layering-architecture.md）：本文件只做**参数解析 + 展示**；
//! 目录拉取（`fetch_directory`）、订阅编排（本地接收准备 + 握手 + watch/重试）
//! 全部收敛在 stross-app 库接口（`subscribe_file`），CLI 不再自带协议客户端。
//!
//! * `ls`：拉取远端节点的 **L2 目录**（类型化 `EndpointDir`）——节点 + 设备 +
//!   可订阅端点（含可见性 / delivery / 传输）；
//! * `subscribe`：订阅远端端点并接收数据。文件端点落盘到 `--out`；
//!   pull（缺省）连公开方中继 watch；push 自建会话 + 自签凭证，公开方
//!   凭凭证出站推入本机中继后 watch 本机中继。
//!
//! 本地双端演示：
//! ```text
//! # 节点 A（公开方）默认端口 serve；节点 B 用自定义端口 + 独立数据目录
//! stross serve --port 18777 --negotiator-port 18779 --data-dir /tmp/stross-a &
//! stross ctrl endpoint publish-file --path ./notes.txt --visibility public --delivery pull
//! stross endpoint ls --host 127.0.0.1 --port 18779
//! stross endpoint subscribe --host 127.0.0.1 --port 18779 \
//!     --endpoint file:notes.txt --out /tmp/stross-files --data-dir /tmp/stross-b
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::{Args, Subcommand};
use stross_kernel::{Kernel, Platform, fetch_directory, subscribe_file};
use stross_proto::message::Delivery;

#[derive(Args, Debug)]
pub struct EndpointArgs {
    /// 远端节点地址（IP / 主机名）
    #[arg(long, global = true, default_value = "127.0.0.1")]
    pub host: String,
    /// 远端节点协商端口（目录 + 订阅握手；本地双端测试按 serve --negotiator-port）
    #[arg(long, global = true, default_value_t = stross_kernel::DEFAULT_NEGOTIATOR_PORT)]
    pub port: u16,
    /// 本机身份数据目录（identity.json；订阅请求携带的本节点 device_id）
    #[arg(long, global = true)]
    pub data_dir: Option<PathBuf>,
    #[command(subcommand)]
    pub command: EndpointCommand,
}

#[derive(Subcommand, Debug)]
pub enum EndpointCommand {
    /// 拉取远端节点目录（L2：节点 + 设备 + 可订阅端点）
    Ls {
        /// 输出原始 JSON（脚本化用）
        #[arg(long)]
        json: bool,
    },
    /// 订阅远端端点并接收（文件端点 → 落盘 --out）
    Subscribe {
        /// 订阅目标端点 id（`ls` 输出里的 endpointId）
        #[arg(long)]
        endpoint: String,
        /// 期望方向（端点声明 Both 时生效；缺省按端点声明）
        #[arg(long, value_enum)]
        delivery: Option<DeliveryArg>,
        /// 文件落盘目录（pull/push 通用）
        #[arg(long, default_value = "stross-files")]
        out: PathBuf,
    },
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum DeliveryArg {
    Pull,
    Push,
}

impl DeliveryArg {
    const fn to_delivery(self) -> Delivery {
        match self {
            Self::Pull => Delivery::Pull,
            Self::Push => Delivery::Push,
        }
    }
}

pub async fn run(args: EndpointArgs) -> anyhow::Result<()> {
    let base = stross_bridge::data_dir(args.data_dir.clone());
    match args.command {
        EndpointCommand::Ls { json } => run_ls(&args.host, args.port, json).await,
        EndpointCommand::Subscribe {
            endpoint,
            delivery,
            out,
        } => {
            let delivery = delivery.map(DeliveryArg::to_delivery);
            run_subscribe(&args.host, args.port, &endpoint, delivery, &out, &base).await
        }
    }
}

/// L2 目录展示（数据拉取走库接口 [`fetch_directory`]，此处仅格式化）。
async fn run_ls(host: &str, port: u16, json: bool) -> anyhow::Result<()> {
    let dir = fetch_directory(host, port).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&dir)?);
        return Ok(());
    }
    println!("节点 {}（{}）", dir.node.device_name, dir.node.device_id);
    println!("已通告端点（{} 个）：", dir.endpoints.len());
    for e in &dir.endpoints {
        let avail = if e.available {
            "可用".to_string()
        } else {
            format!(
                "不可用（{}）",
                e.last_error.as_deref().unwrap_or("未知原因")
            )
        };
        println!(
            "  {}「{}」{} vis={} delivery={} state={}",
            e.endpoint_id,
            e.name,
            avail,
            serde_json::to_string(&e.visibility).unwrap_or_default(),
            serde_json::to_string(&e.delivery).unwrap_or_default(),
            serde_json::to_string(&e.state).unwrap_or_default(),
        );
    }
    println!("订阅: stross endpoint subscribe --endpoint <id> [--delivery pull|push]");
    Ok(())
}

/// 订阅远端端点并接收文件（全流程在库接口 [`subscribe_file`]）。
async fn run_subscribe(
    host: &str,
    port: u16,
    endpoint_id: &str,
    delivery_wish: Option<Delivery>,
    out: &std::path::Path,
    base: &std::path::Path,
) -> anyhow::Result<()> {
    let app = Arc::new(Kernel::new(Platform::Desktop));
    let wanted = delivery_wish.map_or_else(|| "按端点声明".into(), |d| format!("{d:?}"));
    tracing::info!("订阅端点 {endpoint_id}（delivery={wanted}，对端 {host}:{port}）");
    let outcome = subscribe_file(&app, base, host, port, endpoint_id, delivery_wish, out)
        .await
        .with_context(|| format!("订阅端点 {endpoint_id} 失败"))?;
    println!(
        "✅ 已接收文件: {}（{} 字节）→ {}（delivery={:?} stream={}）",
        outcome.received.name,
        outcome.received.size,
        outcome.received.path.display(),
        outcome.delivery,
        outcome.stream_id,
    );
    Ok(())
}
