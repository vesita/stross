//! `stross endpoint`：端点框架（docs/endpoint-model.md）的**订阅方交互面**。
//!
//! * `ls`：拉取远端节点的 **L2 目录**（`GET /api/endpoints`）——节点 + 设备 +
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
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use stross_app::{Platform, ShareGrant, StrossApp, bootstrap};
use stross_proto::message::{Delivery, MediaKind};

use crate::devices::http_get;

#[derive(Args, Debug)]
pub struct EndpointArgs {
    /// 远端节点地址（IP / 主机名）
    #[arg(long, global = true, default_value = "127.0.0.1")]
    pub host: String,
    /// 远端节点协商端口（目录 + 订阅握手；本地双端测试按 serve --negotiator-port）
    #[arg(long, global = true, default_value_t = stross_app::DEFAULT_NEGOTIATOR_PORT)]
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
    fn to_delivery(self) -> Delivery {
        match self {
            DeliveryArg::Pull => Delivery::Pull,
            DeliveryArg::Push => Delivery::Push,
        }
    }
}

pub async fn run(args: EndpointArgs) -> anyhow::Result<()> {
    let base = base_dir(args.data_dir.clone());
    match args.command {
        EndpointCommand::Ls { json } => run_ls(&args.host, args.port, json).await,
        EndpointCommand::Subscribe {
            endpoint,
            delivery,
            out,
        } => {
            run_subscribe(
                &args.host,
                args.port,
                &endpoint,
                delivery.map(|d| d.to_delivery()),
                &out,
                &base,
            )
            .await
        }
    }
}

/// 数据目录（与 serve 同源：--data-dir 优先，否则 XDG/HOME）。
fn base_dir(data_dir: Option<PathBuf>) -> PathBuf {
    if let Some(d) = data_dir {
        return d;
    }
    std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|h| std::path::Path::new(&h).join(".local/share/stross"))
                .unwrap_or_else(|_| PathBuf::from("stross-data"))
        })
}

/// L2 目录拉取：GET /api/endpoints。
async fn run_ls(host: &str, port: u16, json: bool) -> anyhow::Result<()> {
    let body: serde_json::Value =
        http_get(host, port, "/api/endpoints", Duration::from_millis(3000))
            .await
            .context("拉取目录失败（对端 serve 的 --negotiator-port 是否一致？）")?;
    if json {
        println!("{}", serde_json::to_string_pretty(&body)?);
        return Ok(());
    }
    let node = &body["node"];
    println!(
        "节点 {}（{}）",
        node["deviceName"].as_str().unwrap_or("?"),
        node["deviceId"].as_str().unwrap_or("?")
    );
    let devices = body["devices"].as_array().cloned().unwrap_or_default();
    println!("设备（{} 台）：", devices.len());
    for d in &devices {
        println!(
            "  {}「{}」（{}）{}",
            d["deviceId"].as_str().unwrap_or("?"),
            d["name"].as_str().unwrap_or("?"),
            d["kind"].as_str().unwrap_or("?"),
            if d["published"].as_bool().unwrap_or(false) {
                "已公开"
            } else {
                ""
            },
        );
    }
    let endpoints = body["endpoints"].as_array().cloned().unwrap_or_default();
    println!("可订阅端点（{} 个）：", endpoints.len());
    for e in &endpoints {
        println!(
            "  {}「{}」vis={} delivery={} state={}",
            e["endpointId"].as_str().unwrap_or("?"),
            e["device"]["name"].as_str().unwrap_or("?"),
            e["visibility"].as_str().unwrap_or("?"),
            e["delivery"].as_str().unwrap_or("?"),
            e["state"].as_str().unwrap_or("?"),
        );
    }
    println!("订阅: stross endpoint subscribe --endpoint <id> [--delivery pull|push]");
    Ok(())
}

/// 订阅远端端点并接收文件（pull：连公开方中继 watch；push：公开方推入自建
/// 本机中继后 watch 本机中继）。
async fn run_subscribe(
    host: &str,
    port: u16,
    endpoint_id: &str,
    delivery_wish: Option<Delivery>,
    out: &std::path::Path,
    base: &std::path::Path,
) -> anyhow::Result<()> {
    let app = Arc::new(StrossApp::new(Platform::Desktop));
    bootstrap::ensure_identity(&app, base);
    let identity = app
        .device_identity()
        .ok_or_else(|| anyhow::anyhow!("身份未初始化"))?;

    // push 意向：先建本机会话 + 自签凭证 + 锚定本机中继（docs §5 凭证修正）
    let local = if matches!(delivery_wish, Some(Delivery::Push)) {
        Some(prepare_local_receiver(&app).await?)
    } else {
        None
    };
    let req = stross_app::ShareRequest {
        device_id: identity.device_id.clone(),
        device_name: identity.device_name.clone(),
        endpoint_id: Some(endpoint_id.to_string()),
        delivery_mode: delivery_wish,
        relay_addr: local.as_ref().map(|l| l.relay_addr.clone()),
        share_token: local.as_ref().map(|l| l.share_token.clone()),
        media: vec![],
    };
    // 订阅握手（Public / Confirm+信任 自动签发；Confirm 首见需对端人工确认）
    let grant: ShareGrant = http_post_json(host, port, &req).await.context(format!(
        "订阅握手失败（端点 {endpoint_id}；Confirm 端点需对端 stross ctrl negotiator-list 确认）"
    ))?;
    let delivery = grant.delivery.unwrap_or(Delivery::Pull);
    tracing::info!(
        "订阅达成: delivery={delivery:?} stream={} transports={:?}",
        grant.view.stream_id,
        grant.transports,
    );

    match delivery {
        Delivery::Pull => {
            let relay = grant.relay.as_ref().ok_or_else(|| {
                anyhow::anyhow!("pull 授予缺少公开方中继地址（公开方未锚定中继）")
            })?;
            let watch_url = format!("ws://{host}:{}", relay.ws_port);
            tracing::info!(
                "pull：连接公开方中继 {watch_url} 接收 {}",
                grant.view.stream_id
            );
            let got = receive_file_retry(&watch_url, &grant.view.stream_id, out).await?;
            println!(
                "✅ 已接收文件: {}（{} 字节）→ {}",
                got.name,
                got.size,
                got.path.display()
            );
        }
        Delivery::Push => {
            let l = local
                .ok_or_else(|| anyhow::anyhow!("push 授予但本机未准备接收（自签凭证缺失）"))?;
            let watch_url = format!("ws://127.0.0.1:{}", l.relay_port);
            tracing::info!(
                "push：公开方将推入本机中继，watch {watch_url} 接收 {}",
                l.stream_id
            );
            let got = receive_file_retry(&watch_url, &l.stream_id, out).await?;
            println!(
                "✅ 已接收文件: {}（{} 字节）→ {}",
                got.name,
                got.size,
                got.path.display()
            );
        }
        Delivery::Both => unreachable!("公开方已定稿，授予不含 Both"),
    }
    Ok(())
}

/// 接收文件（对「流尚未出现」重试）：
/// 订阅方 watch 与公开方泵建流存在竞态（授予响应先于流注册到达），
/// pump 侧同样在等观看者（docs §5），写满一个 9s 窗口内的重试即稳定收敛。
async fn receive_file_retry(
    watch_url: &str,
    stream_id: &str,
    out: &std::path::Path,
) -> anyhow::Result<stross_app::ReceivedFile> {
    let deadline = Instant::now() + Duration::from_secs(9);
    loop {
        match stross_app::receive_file(watch_url, stream_id, out).await {
            Ok(got) => return Ok(got),
            Err(e) => {
                // 只在「流不存在」（建流竞态）时重试；其它错误（中途断开、
                // 文件不完整）是真实失败，直接上报
                if format!("{e:#}").contains("不存在") && Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                return Err(e);
            }
        }
    }
}

/// push 模式本机准备：锚定受控中继 + 建会话 + 自签一次性凭证。
struct LocalReceiver {
    relay_addr: String,
    relay_port: u16,
    /// 本机自签会话（= 数据面流 id；公开方出站推的就是它）。
    stream_id: String,
    share_token: String,
}

async fn prepare_local_receiver(app: &Arc<StrossApp>) -> anyhow::Result<LocalReceiver> {
    let relay = app.start_relay_on(0).await?;
    let view =
        app.issue_share_token_for("订阅接收文件".into(), vec![MediaKind::File], Some(600))?;
    let ip = advertise_ip();
    Ok(LocalReceiver {
        relay_addr: format!("ws://{ip}:{}", relay.port),
        relay_port: relay.port,
        stream_id: view.stream_id,
        share_token: view.token,
    })
}

/// 广告用本机 IP：优先第一个**非 fake-IP** 的 IPv4（Mihomo/Clash TUN 的
/// 198.18.0.0/15 是路由表占位，连不通），跳过链路本地；全不合格回退回环。
/// 与发现层选址同源决策（AGENTS.md §6 已知坑）。
fn advertise_ip() -> String {
    for ip in stross_core::net::local_ips() {
        let o = match ip {
            std::net::IpAddr::V4(v4) => v4.octets(),
            std::net::IpAddr::V6(_) => continue,
        };
        let is_fake = o[0] == 198 && o[1] == 18;
        let is_link_local = o[0] == 169 && o[1] == 254;
        if !is_fake && !is_link_local {
            return ip.to_string();
        }
    }
    "127.0.0.1".into()
}

/// 极简 HTTP POST（JSON 请求体；raw TCP，无新依赖）。
async fn http_post_json(
    host: &str,
    port: u16,
    req: &stross_app::ShareRequest,
) -> anyhow::Result<ShareGrant> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let body = serde_json::to_vec(req)?;
    let timeout = Duration::from_secs(5);
    let mut stream = tokio::time::timeout(timeout, tokio::net::TcpStream::connect((host, port)))
        .await
        .context("连接协商端点失败")??;
    let head = format!(
        "POST /api/negotiator/request HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    tokio::time::timeout(timeout, stream.write_all(head.as_bytes()))
        .await
        .context("发送请求头失败")??;
    tokio::time::timeout(timeout, stream.write_all(&body))
        .await
        .context("发送请求体失败")??;
    let mut buf = Vec::new();
    tokio::time::timeout(timeout, stream.read_to_end(&mut buf))
        .await
        .context("读取响应失败")??;
    let text = String::from_utf8_lossy(&buf);
    let (status_line, rest) = text
        .split_once("\r\n")
        .ok_or_else(|| anyhow::anyhow!("响应格式非法"))?;
    let body_json = rest.split("\r\n\r\n").nth(1).unwrap_or("");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("500")
        .parse::<u16>()
        .unwrap_or(500);
    if status != 200 {
        let err = serde_json::from_str::<serde_json::Value>(body_json)
            .ok()
            .and_then(|v| v["error"].as_str().map(str::to_string))
            .unwrap_or_else(|| format!("HTTP {status}"));
        bail!("订阅被拒: {err}");
    }
    serde_json::from_str(body_json).context("授予响应解析失败")
}
