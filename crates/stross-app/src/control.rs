//! 控制面（D7）：常驻实例暴露 `/ws/ctrl`，CLI 可像客户端一样接入异步控制。
//!
//! * **请求**：JSON 文本（[`CtrlRequest`]），响应 JSON（[`CtrlResponse`]）；
//! * **事件**：连接即订阅，[`KernelEvent`] 以 JSON 文本推送（异步感知，
//!   如 `StreamStarted` / `StreamEnded` / `WatchersChanged`）；
//! * **安全（D7 v1）**：**仅绑定回环 127.0.0.1**，信任边界 = 本机 OS 用户，
//!   LAN 零暴露；远程控制（配对令牌 + 高危动作用户确认）留待后续阶段。
//!
//! 控制面不携带媒体内容（媒体走数据面，会话 PIN 门控 F2.5）。

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::get;
use serde::{Deserialize, Serialize};
use serde_json::json;
use stross_media::pipeline::StreamConfig;

use crate::app::StrossApp;

/// 控制面默认端口（回环）。
pub const DEFAULT_CTRL_PORT: u16 = 18778;

/// 控制请求（CLI → 实例）。
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "camelCase")]
pub enum CtrlRequest {
    /// 建会话（内核签发 session id，D4）；`sinks` = 接收端节点 id。
    CreateSession { title: String, sinks: Vec<String> },
    /// 会话级访问码鉴权（F2.5）；`access_code = None` 表示无 PIN 会话。
    Authorize {
        session_id: String,
        access_code: Option<String>,
    },
    /// 拆会话（同时拆流）。
    Teardown { session_id: String },
    /// 开始推流（配置即 [`StreamConfig`] 序列化）。
    StartStream {
        config: StreamConfig,
        relay_url: Option<String>,
    },
    /// 停止推流。
    StopStream,
    /// 列出会话。
    ListSessions,
    /// 实例状态（中继端口 / 是否推流 / 会话数）。
    Status,
}

/// 控制响应。`rsp` 标签与 [`crate::kernel::KernelEvent`] 的 `type` 标签区分开，
/// 客户端据此把"响应"与"事件"分开处理。
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "rsp", rename_all = "camelCase")]
pub enum CtrlResponse {
    Ok { payload: serde_json::Value },
    Err { message: String },
}

impl CtrlResponse {
    pub fn ok(payload: serde_json::Value) -> Self {
        Self::Ok { payload }
    }
    pub fn err(message: impl Into<String>) -> Self {
        Self::Err {
            message: message.into(),
        }
    }
}

/// 控制面共享状态。
struct CtrlState {
    app: Arc<StrossApp>,
}

/// 控制面服务器（仅回环，D7）。
pub struct CtrlServer {
    task: tokio::task::JoinHandle<()>,
    /// 实际端口（`port = 0` 时随机）。
    pub port: u16,
}

impl CtrlServer {
    /// 在回环地址上启动控制面。`port = 0` = 随机端口。
    pub async fn start(app: Arc<StrossApp>, port: u16) -> anyhow::Result<Self> {
        let state = Arc::new(CtrlState { app });
        let router = Router::new()
            .route("/ws/ctrl", get(ws_ctrl))
            .with_state(state);
        // D7 v1：仅回环，LAN 零暴露
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .context("绑定控制端口失败")?;
        let actual = listener.local_addr()?.port();
        let task = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, router).await {
                tracing::error!("控制面服务退出: {e}");
            }
        });
        tracing::info!("控制面已启动（仅回环）: ws://127.0.0.1:{actual}/ws/ctrl");
        Ok(Self { task, port: actual })
    }

    /// 停止控制面。
    pub async fn stop(self) {
        self.task.abort();
    }
}

async fn ws_ctrl(ws: WebSocketUpgrade, State(state): State<Arc<CtrlState>>) -> Response {
    ws.on_upgrade(move |socket| handle_client(socket, state))
}

async fn handle_client(mut ws: WebSocket, state: Arc<CtrlState>) {
    let mut events = state.app.kernel().subscribe();
    loop {
        tokio::select! {
            msg = ws.recv() => match msg {
                Some(Ok(Message::Text(text))) => {
                    let resp = handle_request(&state.app, &text).await;
                    let json = serde_json::to_string(&resp).unwrap_or_else(|_| {
                        r#"{"rsp":"error","message":"响应序列化失败"}"#.into()
                    });
                    if ws.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                _ => {}
            },
            // 事件推送：任何控制客户端都能感知实例的异步变化
            ev = events.recv() => match ev {
                Ok(event) => {
                    if let Ok(json) = serde_json::to_string(&event)
                        && ws.send(Message::Text(json.into())).await.is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            },
        }
    }
}

async fn handle_request(app: &StrossApp, text: &str) -> CtrlResponse {
    let req: CtrlRequest = match serde_json::from_str(text) {
        Ok(r) => r,
        Err(e) => return CtrlResponse::err(format!("非法请求: {e}")),
    };
    match req {
        CtrlRequest::CreateSession { title: _, sinks } => {
            // 源节点固定为本机（register_local_node 注册的 "local"）
            match app
                .kernel()
                .create_session("local", &sinks, &Default::default())
                .await
            {
                Ok(s) => CtrlResponse::ok(json!({ "sessionId": s.id })),
                Err(e) => CtrlResponse::err(e),
            }
        }
        CtrlRequest::Authorize {
            session_id,
            access_code,
        } => match app.kernel().authorize(&session_id, access_code.as_deref()) {
            Ok(()) => CtrlResponse::ok(json!({ "sessionId": session_id, "authorized": true })),
            Err(e) => CtrlResponse::err(e),
        },
        CtrlRequest::Teardown { session_id } => match app.kernel().teardown(&session_id).await {
            Ok(()) => CtrlResponse::ok(json!({ "sessionId": session_id })),
            Err(e) => CtrlResponse::err(e),
        },
        CtrlRequest::StartStream { config, relay_url } => {
            match app.start_stream(config, relay_url).await {
                Ok(r) => CtrlResponse::ok(json!({
                    "relayPort": r.relay_port,
                    "watchUrls": r.watch_urls,
                    "streamId": r.stream_id,
                })),
                Err(e) => CtrlResponse::err(e),
            }
        }
        CtrlRequest::StopStream => match app.stop_stream().await {
            Ok(()) => CtrlResponse::ok(json!({ "stopped": true })),
            Err(e) => CtrlResponse::err(e),
        },
        CtrlRequest::ListSessions => {
            let sessions: Vec<serde_json::Value> = app
                .kernel()
                .sessions()
                .iter()
                .map(|s| {
                    json!({
                        "sessionId": s.id,
                        "source": s.source,
                        "sinks": s.sinks,
                        "requiresPin": s.requires_pin,
                    })
                })
                .collect();
            CtrlResponse::ok(json!({ "sessions": sessions }))
        }
        CtrlRequest::Status => {
            let status = app.stream_status();
            CtrlResponse::ok(json!({
                "relayPort": app.relay_port().unwrap_or_else(|| app.stream_relay_port()),
                "streaming": status.running,
                "sessions": app.kernel().sessions().len(),
            }))
        }
    }
}
