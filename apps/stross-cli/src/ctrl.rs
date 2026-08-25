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
use stross_media::pipeline::{AudioSourceConfig, StreamConfig, VideoSource};
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
    /// 实例状态（中继端口 / 是否推流 / 会话数）
    Status,
    /// 订阅并打印内核事件（异步感知）
    Events {
        #[arg(long, default_value_t = 5)]
        secs: u64,
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
        CtrlCommand::StartStream {
            secs,
            audio,
            stream_id,
            quality,
            relay,
        } => {
            let mut config = StreamConfig {
                stream_id: stream_id.unwrap_or_else(|| format!("demo-{}", std::process::id())),
                title: "CLI 推流".into(),
                video: Some(VideoSource::Synthetic {
                    pattern: "testsrc2".into(),
                }),
                quality: quality.quality(),
                audio: None,
                duration_secs: Some(secs as u32),
            };
            if audio {
                config.audio = Some(AudioSourceConfig::synthetic_test());
            }
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
        CtrlCommand::Status => {
            let req = CtrlRequest::Status;
            let payload = request(&args.connect, req).await?;
            tracing::info!("状态: {payload}");
        }
        CtrlCommand::Events { secs } => events(&args.connect, secs).await?,
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
