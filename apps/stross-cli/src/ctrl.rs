//! `stross ctrl`：接入运行中 Stross 实例的控制面（D7），异步下发控制命令。
//!
//! 模型：`stross serve`（或 GUI）常驻运行 → 本命令像客户端一样接上控制。
//! 控制面仅回环绑定，信任边界 = 本机用户。
//!
//! ```text
//! stross ctrl create-session --title demo                      # → sessionId
//! stross ctrl start-stream --stream-id <sid> --audio --secs 10 # 起流
//! stross ctrl events --secs 5                                  # 订阅事件
//! stross ctrl teardown <sid>
//! ```

use anyhow::bail;
use clap::{Args, Subcommand};
use stross_endpoint::pipeline::StreamConfig;
use stross_kernel::CtrlRequest;
use stross_proto::message::{Delivery, Visibility};

use crate::push::QualityArg;

/// 控制面默认地址（与 `stross serve` 的默认 ctrl 端口一致；端口真源在
/// `stross_kernel::DEFAULT_CTRL_PORT`，壳层不得硬编码端口号）。
fn default_ctrl_url() -> String {
    format!(
        "ws://127.0.0.1:{}/ws/ctrl",
        stross_kernel::DEFAULT_CTRL_PORT
    )
}

#[derive(Args, Debug)]
pub struct CtrlArgs {
    /// 控制面地址（仅回环，D7）
    #[arg(long, default_value_t = default_ctrl_url())]
    pub connect: String,
    #[command(subcommand)]
    pub command: CtrlCommand,
}

#[derive(Subcommand, Debug)]
pub enum CtrlCommand {
    /// 建会话（内核签发 session id，D4）
    CreateSession {
        #[arg(long, default_value = "ctrl")]
        title: String,
        /// 接收端节点 id（逗号分隔；默认本机 local）
        #[arg(long, default_value = "local")]
        sinks: String,
    },
    /// 会话级访问码鉴权（F2.5）
    Authorize {
        session_id: String,
        #[arg(long)]
        code: Option<String>,
    },
    /// 拆会话（同时拆流）
    Teardown { session_id: String },
    /// 为会话签发一次性接入凭证（跨设备推流用）：把返回的 token 交给推流端
    /// （如手机 `stross push --share-token <token> --relay ws://本机IP:端口/ws/push`），
    /// 推流端凭此直接接入本机受控中继，无需远程控制面
    ShareToken {
        session_id: String,
        /// 有效期（秒，默认 300）
        #[arg(long, default_value_t = 300)]
        ttl: u64,
    },
    /// 开始推流（合成源，无设备环境可跑）
    StartStream {
        #[arg(long, default_value_t = 15)]
        secs: u64,
        #[arg(long)]
        audio: bool,
        #[arg(long)]
        stream_id: Option<String>,
        #[arg(long, value_enum, default_value_t = QualityArg::Medium)]
        quality: QualityArg,
        /// 推往外部中继（默认内嵌）
        #[arg(long)]
        relay: Option<String>,
    },
    /// 停止推流
    StopStream,
    /// 列出会话
    ListSessions,
    /// 实例状态（版本/uptime/中继端口/推流/会话数）
    Status {
        /// 输出原始 JSON（脚本化用）
        #[arg(long)]
        json: bool,
    },
    /// 订阅并打印内核事件（异步感知）
    Events {
        #[arg(long, default_value_t = 5)]
        secs: u64,
    },
    /// 列出待人工确认的凭证协商请求（serve 启用协商端点时可用）
    NegotiatorList {
        /// 输出原始 JSON（脚本化用）
        #[arg(long)]
        json: bool,
    },
    /// 应答凭证协商请求（允许=签发接入凭证并通知申请方）
    NegotiatorRespond {
        /// 挂起请求 id（negotiator-list 返回）
        req_id: String,
        /// 拒绝（缺省为允许）
        #[arg(long)]
        deny: bool,
        /// 记住该设备（下次自动签发，免确认）
        #[arg(long)]
        remember: bool,
    },
    /// 端点框架命令（公开 / 取消公开 / 目录；docs/endpoint-model.md）
    Endpoint {
        #[command(subcommand)]
        cmd: EndpointCommand,
    },
}

/// `stross ctrl endpoint` 子命令。
#[derive(Subcommand, Debug)]
pub enum EndpointCommand {
    /// 公开设备为端点（端点框架 docs/endpoint-model.md；P1 一设备一端点）
    Publish {
        /// 设备 id（`stross ctrl endpoint list` 可查）
        #[arg(long)]
        device: String,
        /// public | confirm | private
        #[arg(long, default_value = "public")]
        visibility: String,
        /// private 白名单（节点 device_id，逗号分隔）
        #[arg(long, value_delimiter = ',')]
        nodes: Vec<String>,
        /// pull | push | both（数据面连接方向由公开者声明）
        #[arg(long, default_value = "pull")]
        delivery: String,
    },
    /// 公开本地文件为文件端点（file:<名> 动态设备）
    PublishFile {
        /// 本地文件路径
        #[arg(long)]
        path: String,
        #[arg(long, default_value = "public")]
        visibility: String,
        #[arg(long, value_delimiter = ',')]
        nodes: Vec<String>,
        #[arg(long, default_value = "pull")]
        delivery: String,
    },
    /// 取消公开端点
    Unpublish { endpoint_id: String },
    /// 列出本节点设备 + 已公开端点
    List {
        /// 输出原始 JSON（脚本化用）
        #[arg(long)]
        json: bool,
    },
}

pub async fn run(args: CtrlArgs) -> anyhow::Result<()> {
    match args.command {
        CtrlCommand::CreateSession { title, sinks } => {
            let req = CtrlRequest::CreateSession {
                title,
                sinks: sinks.split(',').map(|s| s.trim().to_string()).collect(),
            };
            let r: stross_types::SessionCreatedView = request_as(&args.connect, req).await?;
            tracing::info!("sessionId: {}", r.session_id);
        }
        CtrlCommand::Authorize { session_id, code } => {
            let req = CtrlRequest::Authorize {
                session_id: session_id.clone(),
                access_code: code,
            };
            let _: stross_types::AuthorizedView = request_as(&args.connect, req).await?;
            tracing::info!("已鉴权: {session_id}");
        }
        CtrlCommand::Teardown { session_id } => {
            let req = CtrlRequest::Teardown {
                session_id: session_id.clone(),
            };
            let _: stross_types::TeardownView = request_as(&args.connect, req).await?;
            tracing::info!("已拆除会话: {session_id}");
        }
        CtrlCommand::ShareToken { session_id, ttl } => {
            let req = CtrlRequest::ShareToken {
                session_id: session_id.clone(),
                ttl_secs: ttl,
            };
            let r: stross_types::IssuedShareTokenView = request_as(&args.connect, req).await?;
            tracing::info!(
                "已签发接入凭证: streamId={} pin={} expiresAt={}",
                r.stream_id,
                r.pin,
                r.expires_at,
            );
            tracing::info!("推流端出示凭证（--share-token）: {}", r.token);
        }
        CtrlCommand::StartStream {
            secs,
            audio,
            stream_id,
            quality,
            relay,
        } => {
            let config = StreamConfig::cli_synthetic(
                stream_id.unwrap_or_else(|| format!("demo-{}", std::process::id())),
                "CLI 推流".into(),
                quality.quality(),
                secs as u32,
                audio,
                None,
            );
            let req = CtrlRequest::StartStream {
                config,
                relay_url: relay,
            };
            let r: stross_types::StartResult = request_as(&args.connect, req).await?;
            tracing::info!(
                "推流已启动: streamId={} relayPort={} watchUrls={:?}",
                r.stream_id,
                r.relay_port,
                r.watch_urls
            );
        }
        CtrlCommand::StopStream => {
            let _: stross_types::StoppedView =
                request_as(&args.connect, CtrlRequest::StopStream).await?;
            tracing::info!("推流已停止");
        }
        CtrlCommand::ListSessions => {
            let r: stross_types::SessionsPayload =
                request_as(&args.connect, CtrlRequest::ListSessions).await?;
            tracing::info!("会话: {}", serde_json::to_string(&r)?);
        }
        CtrlCommand::Status { json } => {
            let r: stross_types::StatusView =
                request_as(&args.connect, CtrlRequest::Status).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
                return Ok(());
            }
            print_status(&r);
        }
        CtrlCommand::Events { secs } => events(&args.connect, secs).await?,
        CtrlCommand::NegotiatorList { json } => {
            let r: stross_types::PendingRequestsPayload =
                request_as(&args.connect, CtrlRequest::NegotiatorPending).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
                return Ok(());
            }
            if r.pending.is_empty() {
                println!("无待确认的协商请求");
                return Ok(());
            }
            println!("待确认的凭证协商请求：");
            for p in &r.pending {
                println!(
                    "  {}  {}（{}）media={}",
                    p.id,
                    p.device_name,
                    p.device_id,
                    p.media.join(","),
                );
            }
            println!(
                "批准: stross ctrl negotiator-respond <id>（--deny 拒绝，--remember 记住设备）"
            );
        }
        CtrlCommand::NegotiatorRespond {
            req_id,
            deny,
            remember,
        } => {
            let r: stross_types::GrantResponseView = request_as(
                &args.connect,
                CtrlRequest::NegotiatorRespond {
                    req_id: req_id.clone(),
                    allow: !deny,
                    remember,
                },
            )
            .await?;
            if deny || r.denied {
                println!("已拒绝请求 {req_id}");
            } else {
                println!(
                    "已允许 {req_id} → streamId={} pin={}",
                    r.stream_id.as_deref().unwrap_or("?"),
                    r.pin.as_deref().unwrap_or("?"),
                );
            }
        }
        CtrlCommand::Endpoint { cmd } => match cmd {
            EndpointCommand::Publish {
                device,
                visibility,
                nodes,
                delivery,
            } => {
                let req = CtrlRequest::EndpointPublish {
                    device_id: device.clone(),
                    visibility: parse_visibility(&visibility, &nodes)?,
                    delivery: parse_delivery(&delivery)?,
                    transports: None,
                    codecs: None,
                };
                let m: stross_types::EndpointPublishedView = request_as(&args.connect, req).await?;
                println!(
                    "已公开端点 {}（{}）delivery={}",
                    m.endpoint_id,
                    m.name,
                    m.delivery.as_str(),
                );
            }
            EndpointCommand::PublishFile {
                path,
                visibility,
                nodes,
                delivery,
            } => {
                let req = CtrlRequest::EndpointPublishFile {
                    path: path.clone(),
                    visibility: parse_visibility(&visibility, &nodes)?,
                    delivery: parse_delivery(&delivery)?,
                };
                let r: stross_types::FilePublishedView = request_as(&args.connect, req).await?;
                println!(
                    "已公开文件端点 {}（{}，{} 字节）delivery={}",
                    r.endpoint_id,
                    r.name,
                    r.size,
                    r.delivery.as_str(),
                );
            }
            EndpointCommand::Unpublish { endpoint_id } => {
                let req = CtrlRequest::EndpointUnpublish {
                    endpoint_id: endpoint_id.clone(),
                };
                let _: stross_types::UnpublishedView = request_as(&args.connect, req).await?;
                println!("已取消公开端点: {endpoint_id}");
            }
            EndpointCommand::List { json } => {
                let r: stross_types::EndpointListPayload =
                    request_as(&args.connect, CtrlRequest::EndpointList).await?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&r)?);
                    return Ok(());
                }
                println!("本节点端点（{} 个）：", r.endpoints.len());
                for e in &r.endpoints {
                    let avail = if e.available {
                        "可用".to_string()
                    } else {
                        format!(
                            "不可用（{}）",
                            e.last_error.as_deref().unwrap_or("未知原因")
                        )
                    };
                    println!(
                        "  {}「{}」{} {}{} vis={} delivery={} state={} subscribers={}",
                        e.endpoint_id,
                        e.name,
                        avail,
                        if e.published {
                            "已通告"
                        } else {
                            "未通告"
                        },
                        e.kind.as_str(),
                        e.visibility.as_str(),
                        e.delivery.as_str(),
                        e.state.as_str(),
                        e.subscribers,
                    );
                }
                println!(
                    "公开: stross ctrl endpoint publish --device <id> --visibility ... --delivery ..."
                );
            }
        },
    }
    Ok(())
}

/// 可见性参数解析（public | confirm | private + 白名单节点）。
fn parse_visibility(s: &str, nodes: &[String]) -> anyhow::Result<Visibility> {
    match Visibility::from_wire(s) {
        Some(Visibility::Private { .. }) => Ok(Visibility::Private {
            nodes: nodes.to_vec(),
        }),
        Some(v) => Ok(v),
        None => bail!("--visibility 取值 public|confirm|private，收到 {s}"),
    }
}

/// delivery 参数解析（pull | push | both）。
fn parse_delivery(s: &str) -> anyhow::Result<Delivery> {
    Delivery::from_wire(s)
        .ok_or_else(|| anyhow::anyhow!("--delivery 取值 pull|push|both，收到 {s}"))
}

/// 发请求并把载荷反序列化为类型化视图（内核产出，壳层不定义响应结构）。
async fn request_as<T: serde::de::DeserializeOwned>(
    connect: &str,
    req: CtrlRequest,
) -> anyhow::Result<T> {
    stross_kernel::control::client::request_as(connect, req).await
}

/// 订阅并打印内核事件（`StreamStarted` / `StreamEnded` / 会话变化等）。
async fn events(connect: &str, secs: u64) -> anyhow::Result<()> {
    let evs = stross_kernel::control::client::collect_events(connect, secs).await?;
    for v in evs {
        tracing::info!("事件: {v}");
    }
    Ok(())
}

/// 人类可读的实例状态输出（`stross ctrl status`）。
fn print_status(s: &stross_types::StatusView) {
    println!("Stross 实例状态");
    println!("  版本      v{} ({})", s.version, s.platform);
    println!("  运行时长  {}", fmt_dur(s.uptime_secs));
    let relay = s.relay_port;
    let transports = match (s.srt_port, s.quic_port) {
        (Some(srt), Some(quic)) => format!("（SRT {srt} · QUIC {quic}）"),
        (Some(srt), None) => format!("（SRT {srt}）"),
        (None, Some(quic)) => format!("（QUIC {quic}）"),
        (None, None) => String::new(),
    };
    println!("  中继      ws://127.0.0.1:{relay}{transports}");
    if s.streaming {
        let sid = s.stream_id.as_deref().unwrap_or("?").to_string();
        let title = s
            .stream_title
            .as_deref()
            .filter(|t| !t.is_empty())
            .unwrap_or("未命名")
            .to_string();
        let since = s.stream_started_at.unwrap_or(0);
        let now = stross_proto::time::unix_secs();
        println!(
            "  推流      运行中 {sid}「{title}」已推 {}",
            fmt_dur(now.saturating_sub(since))
        );
    } else {
        println!("  推流      未运行");
    }
    println!("  会话数    {}", s.sessions);
}

/// 秒数 → "X 分 Y 秒" / "Y 秒"。
fn fmt_dur(total_secs: u64) -> String {
    let m = total_secs / 60;
    let s = total_secs % 60;
    let mut out = String::new();
    if m > 0 {
        out.push_str(&format!("{m} 分 "));
    }
    out.push_str(&format!("{s} 秒"));
    out
}
