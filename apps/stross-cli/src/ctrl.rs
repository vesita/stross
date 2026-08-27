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

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use futures_util::{SinkExt, StreamExt};
use stross_app::CtrlRequest;
use stross_media::pipeline::StreamConfig;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::push::QualityArg;

/// 控制面默认地址（与 stross serve 的默认 ctrl 端口一致）。
pub const DEFAULT_CTRL_URL: &str = "ws://127.0.0.1:18778/ws/ctrl";

#[derive(Args, Debug)]
pub struct CtrlArgs {
    /// 控制面地址（仅回环，D7）
    #[arg(long, default_value = DEFAULT_CTRL_URL)]
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
}

pub async fn run(args: CtrlArgs) -> anyhow::Result<()> {
    match args.command {
        CtrlCommand::CreateSession { title, sinks } => {
            let req = CtrlRequest::CreateSession {
                title,
                sinks: sinks.split(',').map(|s| s.trim().to_string()).collect(),
            };
            let payload = request(&args.connect, req).await?;
            tracing::info!(
                "sessionId: {}",
                payload["sessionId"].as_str().unwrap_or("?")
            );
        }
        CtrlCommand::Authorize { session_id, code } => {
            let req = CtrlRequest::Authorize {
                session_id: session_id.clone(),
                access_code: code,
            };
            let _ = request(&args.connect, req).await?;
            tracing::info!("已鉴权: {session_id}");
        }
        CtrlCommand::Teardown { session_id } => {
            let req = CtrlRequest::Teardown {
                session_id: session_id.clone(),
            };
            let _ = request(&args.connect, req).await?;
            tracing::info!("已拆除会话: {session_id}");
        }
        CtrlCommand::ShareToken { session_id, ttl } => {
            let req = CtrlRequest::ShareToken {
                session_id: session_id.clone(),
                ttl_secs: ttl,
            };
            let payload = request(&args.connect, req).await?;
            let token = payload["token"].as_str().unwrap_or("?");
            tracing::info!(
                "已签发接入凭证: streamId={} pin={} expiresAt={}",
                payload["streamId"].as_str().unwrap_or("?"),
                payload["pin"].as_str().unwrap_or("?"),
                payload["expiresAt"],
            );
            tracing::info!("推流端出示凭证（--share-token）: {token}");
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
            let payload = request(&args.connect, req).await?;
            tracing::info!(
                "推流已启动: streamId={} relayPort={} watchUrls={:?}",
                payload["streamId"].as_str().unwrap_or("?"),
                payload["relayPort"],
                payload["watchUrls"]
            );
        }
        CtrlCommand::StopStream => {
            let req = CtrlRequest::StopStream;
            let _ = request(&args.connect, req).await?;
            tracing::info!("推流已停止");
        }
        CtrlCommand::ListSessions => {
            let req = CtrlRequest::ListSessions;
            let payload = request(&args.connect, req).await?;
            tracing::info!("会话: {payload}");
        }
        CtrlCommand::Status { json } => {
            let req = CtrlRequest::Status;
            let payload = request(&args.connect, req).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&payload)?);
                return Ok(());
            }
            print_status(&payload);
        }
        CtrlCommand::Events { secs } => events(&args.connect, secs).await?,
        CtrlCommand::NegotiatorList { json } => {
            let payload = request(&args.connect, CtrlRequest::NegotiatorPending).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&payload)?);
                return Ok(());
            }
            let pending = payload["pending"].as_array().cloned().unwrap_or_default();
            if pending.is_empty() {
                println!("无待确认的协商请求");
                return Ok(());
            }
            println!("待确认的凭证协商请求：");
            for p in &pending {
                println!(
                    "  {}  {}（{}）media={}",
                    p["id"].as_str().unwrap_or("?"),
                    p["deviceName"].as_str().unwrap_or("?"),
                    p["deviceId"].as_str().unwrap_or("?"),
                    p["media"]
                        .as_array()
                        .map(|m| m
                            .iter()
                            .filter_map(|x| x.as_str())
                            .collect::<Vec<_>>()
                            .join(","))
                        .unwrap_or_default(),
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
            let payload = request(
                &args.connect,
                CtrlRequest::NegotiatorRespond {
                    req_id: req_id.clone(),
                    allow: !deny,
                    remember,
                },
            )
            .await?;
            if deny {
                println!("已拒绝请求 {req_id}");
            } else {
                println!(
                    "已允许 {req_id} → streamId={} pin={}",
                    payload["streamId"].as_str().unwrap_or("?"),
                    payload["pin"].as_str().unwrap_or("?")
                );
            }
        }
    }
    Ok(())
}

/// 发一个请求并等待响应（忽略事件推送）。
async fn request(connect: &str, req: CtrlRequest) -> anyhow::Result<serde_json::Value> {
    let (mut ws, _) = connect_async(connect)
        .await
        .context("连接控制面失败（实例是否在运行？）")?;
    ws.send(Message::Text(serde_json::to_string(&req)?.into()))
        .await?;
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(text))) => {
                let v: serde_json::Value = serde_json::from_str(&text)?;
                match v.get("rsp").and_then(|x| x.as_str()) {
                    Some("ok") => return Ok(v["payload"].clone()),
                    Some("error") => {
                        bail!("{}", v["message"].as_str().unwrap_or("未知错误"))
                    }
                    _ => {} // KernelEvent（type 标签），忽略
                }
            }
            Some(Ok(Message::Close(_))) | None => bail!("控制面连接关闭"),
            _ => {}
        }
    }
}

/// 订阅并打印内核事件（`StreamStarted` / `StreamEnded` / 会话变化等）。
async fn events(connect: &str, secs: u64) -> anyhow::Result<()> {
    let (mut ws, _) = connect_async(connect)
        .await
        .context("连接控制面失败（实例是否在运行？）")?;
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            msg = ws.next() => match msg {
                Some(Ok(Message::Text(text))) => {
                    let v: serde_json::Value = serde_json::from_str(&text)?;
                    // 事件无 rsp 标签；响应（rsp）忽略
                    if v.get("rsp").is_none() {
                        tracing::info!("事件: {v}");
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                _ => {}
            },
        }
    }
    Ok(())
}

/// 人类可读的实例状态输出（`stross ctrl status`）。
fn print_status(p: &serde_json::Value) {
    let get = |k: &str| p.get(k).cloned().unwrap_or(serde_json::Value::Null);
    println!("Stross 实例状态");
    println!(
        "  版本      v{} ({})",
        get("version").as_str().unwrap_or("?"),
        get("platform").as_str().unwrap_or("?")
    );
    println!(
        "  运行时长  {}",
        fmt_dur(get("uptimeSecs").as_u64().unwrap_or(0))
    );
    let relay = get("relayPort").as_u64().unwrap_or(0);
    let transports = match (get("srtPort").as_u64(), get("quicPort").as_u64()) {
        (Some(s), Some(q)) => format!("（SRT {s} · QUIC {q}）"),
        (Some(s), None) => format!("（SRT {s}）"),
        (None, Some(q)) => format!("（QUIC {q}）"),
        (None, None) => String::new(),
    };
    println!("  中继      ws://127.0.0.1:{relay}{transports}");
    if get("streaming").as_bool().unwrap_or(false) {
        let sid = get("streamId").as_str().unwrap_or("?").to_string();
        let title = get("streamTitle")
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("未命名")
            .to_string();
        let since = get("streamStartedAt").as_u64().unwrap_or(0);
        let now = stross_proto::time::unix_secs();
        println!(
            "  推流      运行中 {sid}「{title}」已推 {}",
            fmt_dur(now.saturating_sub(since))
        );
    } else {
        println!("  推流      未运行");
    }
    println!("  会话数    {}", get("sessions").as_u64().unwrap_or(0));
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
