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
use stross_proto::message::{Delivery, TransportPreference, Visibility};

use crate::Kernel;
use crate::SessionPrefs;
use crate::negotiator::ShareNegotiator;

/// 控制面默认端口（回环）。
pub const DEFAULT_CTRL_PORT: u16 = 18778;

/// 接入凭证默认有效期（秒，5 分钟）。
const fn default_token_ttl() -> u64 {
    300
}

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
    /// 为已建会话签发一次性接入凭证（B 阶段：跨设备推流）。
    /// 返回 [`stross_proto::message::ShareToken`] 字符串 + PIN，调用方编码为
    /// 二维码 / 短码交给推流端；凭证经推流端 Hello 出示即接入本机受控中继。
    ShareToken {
        session_id: String,
        /// 有效期（秒）。
        #[serde(default = "default_token_ttl")]
        ttl_secs: u64,
    },
    /// 停止推流。
    StopStream,
    /// 列出会话。
    ListSessions,
    /// 实例状态（中继端口 / 是否推流 / 会话数）。
    Status,
    /// 列出待人工确认的凭证协商请求（serve 启用协商端点时可用）。
    NegotiatorPending,
    /// 应答协商请求（`allow = true` 签发凭证；`remember` 顺带记住该设备）。
    NegotiatorRespond {
        req_id: String,
        allow: bool,
        #[serde(default)]
        remember: bool,
    },
    /// 公开设备为端点（端点框架，docs/endpoint-model.md §6；P1 1:1）。
    EndpointPublish {
        device_id: String,
        visibility: Visibility,
        delivery: Delivery,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transports: Option<Vec<TransportPreference>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codecs: Option<Vec<stross_proto::message::CodecId>>,
    },
    /// 公开本地文件为文件端点（动态设备 `file:<名>`）。
    EndpointPublishFile {
        path: String,
        visibility: Visibility,
        delivery: Delivery,
    },
    /// 取消公开端点。
    EndpointUnpublish { endpoint_id: String },
    /// 目录快照（本节点设备 + 已公开端点）。
    EndpointList,
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
    app: Arc<Kernel>,
    /// 凭证协商服务句柄（serve 启动时注入；None = 未启用）。
    negotiator: Option<Arc<ShareNegotiator>>,
}

/// 控制面服务器（仅回环，D7）。
pub struct CtrlServer {
    task: tokio::task::JoinHandle<()>,
    /// 实际端口（`port = 0` 时随机）。
    pub port: u16,
}

impl CtrlServer {
    /// 在回环地址上启动控制面。`port = 0` = 随机端口。
    ///
    /// `negotiator`：可选的凭证协商服务句柄——提供则控制面暴露
    /// `NegotiatorPending` / `NegotiatorRespond` 命令（CLI 审批接入请求）。
    pub async fn start(
        app: Arc<Kernel>,
        port: u16,
        negotiator: Option<Arc<ShareNegotiator>>,
    ) -> anyhow::Result<Self> {
        let state = Arc::new(CtrlState { app, negotiator });
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
    let mut events = state.app.subscribe();
    loop {
        tokio::select! {
            msg = ws.recv() => match msg {
                Some(Ok(Message::Text(text))) => {
                    let resp = handle_request(&state, &text).await;
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

async fn handle_request(state: &CtrlState, text: &str) -> CtrlResponse {
    let app = &state.app;
    let req: CtrlRequest = match serde_json::from_str(text) {
        Ok(r) => r,
        Err(e) => return CtrlResponse::err(format!("非法请求: {e}")),
    };
    match req {
        CtrlRequest::NegotiatorPending => {
            let Some(neg) = &state.negotiator else {
                return CtrlResponse::err("凭证协商服务未启动（serve 未启用协商端点）");
            };
            let pending: Vec<serde_json::Value> = neg
                .pending_requests()
                .iter()
                .map(|r| {
                    json!({
                        "id": r.id,
                        "deviceId": r.device_id,
                        "deviceName": r.device_name,
                        "media": r.media,
                        "createdAt": r.created_at,
                    })
                })
                .collect();
            CtrlResponse::ok(json!({ "pending": pending }))
        }
        CtrlRequest::NegotiatorRespond {
            req_id,
            allow,
            remember,
        } => {
            let Some(neg) = &state.negotiator else {
                return CtrlResponse::err("凭证协商服务未启动（serve 未启用协商端点）");
            };
            match neg.respond(&req_id, allow, remember) {
                Ok(Some(grant)) => CtrlResponse::ok(json!({
                    "streamId": grant.view.stream_id,
                    "pin": grant.view.pin,
                    "expiresAt": grant.view.expires_at,
                    "trusted": grant.trusted,
                })),
                Ok(None) => CtrlResponse::ok(json!({ "denied": true })),
                Err(e) => CtrlResponse::err(e),
            }
        }
        CtrlRequest::EndpointPublish {
            device_id,
            visibility,
            delivery,
            transports,
            codecs,
        } => match app.publish_endpoint(&device_id, visibility, delivery, transports, codecs) {
            Ok(m) => CtrlResponse::ok(json!({
                "endpointId": m.endpoint_id,
                "deviceName": m.device.name,
                "delivery": serde_json::to_string(&m.delivery).unwrap_or_default(),
            })),
            Err(e) => CtrlResponse::err(e.to_user_string()),
        },
        CtrlRequest::EndpointPublishFile {
            path,
            visibility,
            delivery,
        } => match app.publish_file_endpoint(std::path::Path::new(&path), visibility, delivery) {
            Ok(m) => CtrlResponse::ok(json!({
                "endpointId": m.endpoint_id,
                "deviceName": m.device.name,
                "size": app.file_source(&m.endpoint_id).map(|s| s.size).unwrap_or(0),
                "delivery": serde_json::to_string(&m.delivery).unwrap_or_default(),
            })),
            Err(e) => CtrlResponse::err(e.to_user_string()),
        },
        CtrlRequest::EndpointUnpublish { endpoint_id } => {
            match app.unpublish_endpoint(&endpoint_id) {
                Ok(()) => {
                    CtrlResponse::ok(json!({ "endpointId": endpoint_id, "unpublished": true }))
                }
                Err(e) => CtrlResponse::err(e.to_user_string()),
            }
        }
        CtrlRequest::EndpointList => {
            let (devices, endpoints) = app.endpoint_catalog();
            let devices: Vec<serde_json::Value> = devices
                .iter()
                .map(|d| {
                    json!({
                        "deviceId": d.device_id,
                        "kind": serde_json::to_string(&d.kind).unwrap_or_default(),
                        "name": d.name,
                        "builtin": d.builtin,
                    })
                })
                .collect();
            let endpoints: Vec<serde_json::Value> = endpoints
                .iter()
                .map(|e| {
                    json!({
                        "endpointId": e.endpoint_id,
                        "deviceId": e.device.device_id,
                        "kind": serde_json::to_string(&e.device.kind).unwrap_or_default(),
                        "name": e.device.name,
                        "visibility": serde_json::to_string(&e.visibility).unwrap_or_default(),
                        "delivery": serde_json::to_string(&e.delivery).unwrap_or_default(),
                        "state": serde_json::to_string(&e.state).unwrap_or_default(),
                        "subscribers": e.subscribers,
                        "transports": e.transports.iter().map(|t| {
                            json!({ "transport": serde_json::to_string(&t.transport).unwrap_or_default(), "priority": t.priority })
                        }).collect::<Vec<_>>(),
                    })
                })
                .collect();
            // 已公开端点 id 集合（供设备行标注「已公开」）——设备表按注册顺序输出
            let published: std::collections::HashSet<&str> = endpoints
                .iter()
                .filter_map(|e| e.get("endpointId").and_then(|x| x.as_str()))
                .collect();
            let devices: Vec<serde_json::Value> = devices
                .into_iter()
                .map(|mut d| {
                    let id = d["deviceId"].as_str().unwrap_or("").to_string();
                    d["published"] = serde_json::json!(published.contains(id.as_str()));
                    d
                })
                .collect();
            CtrlResponse::ok(json!({ "devices": devices, "endpoints": endpoints }))
        }
        CtrlRequest::CreateSession { title, sinks } => {
            // 源节点固定为本机（register_local_node 注册的 "local"）；
            // title 随会话存储（UI 展示），不再是死字段
            let prefs = SessionPrefs {
                title,
                ..Default::default()
            };
            match app.create_session("local", &sinks, &prefs) {
                Ok(s) => CtrlResponse::ok(json!({ "sessionId": s.id, "title": s.title })),
                Err(e) => CtrlResponse::err(e.to_user_string()),
            }
        }
        CtrlRequest::Authorize {
            session_id,
            access_code,
        } => match app.authorize(&session_id, access_code.as_deref()) {
            Ok(()) => CtrlResponse::ok(json!({ "sessionId": session_id, "authorized": true })),
            Err(e) => CtrlResponse::err(e.to_user_string()),
        },
        CtrlRequest::Teardown { session_id } => match app.teardown(&session_id) {
            Ok(()) => CtrlResponse::ok(json!({ "sessionId": session_id })),
            Err(e) => CtrlResponse::err(e.to_user_string()),
        },
        CtrlRequest::StartStream { config, relay_url } => {
            match app.start_stream(config, relay_url).await {
                Ok(r) => CtrlResponse::ok(json!({
                    "relayPort": r.relay_port,
                    "watchUrls": r.watch_urls,
                    "streamId": r.stream_id,
                })),
                Err(e) => CtrlResponse::err(e.to_user_string()),
            }
        }
        CtrlRequest::ShareToken {
            session_id,
            ttl_secs,
        } => {
            // D3 场景：接收端默认接受手机麦克风反向共享
            let media = vec![stross_proto::message::MediaKind::Mic];
            match app.create_share_token(
                &session_id,
                media,
                std::time::Duration::from_secs(ttl_secs),
            ) {
                Ok(token) => CtrlResponse::ok(json!({
                    "token": token.to_token_string(),
                    "streamId": token.stream_id,
                    "pin": token.pin,
                    "expiresAt": token.expires_at,
                    "media": token.media.iter().map(|m| format!("{m:?}")).collect::<Vec<_>>(),
                })),
                Err(e) => CtrlResponse::err(e.to_user_string()),
            }
        }
        CtrlRequest::StopStream => match app.stop_stream().await {
            Ok(()) => CtrlResponse::ok(json!({ "stopped": true })),
            Err(e) => CtrlResponse::err(e.to_user_string()),
        },
        CtrlRequest::ListSessions => {
            let sessions: Vec<serde_json::Value> = app
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
            let (ws_port, srt_port, quic_port) =
                app.relay_ports()
                    .unwrap_or((app.stream_relay_port(), None, None));
            CtrlResponse::ok(json!({
                "version": env!("CARGO_PKG_VERSION"),
                "platform": app.platform_str(),
                "uptimeSecs": app.uptime_secs(),
                "relayPort": ws_port,
                "srtPort": srt_port,
                "quicPort": quic_port,
                "streaming": status.running,
                "streamId": status.stream_id,
                "streamTitle": status.title,
                "streamStartedAt": status.started_at,
                "sessions": app.sessions().len(),
            }))
        }
    }
}

/// 控制面客户端（回环 WS）：请求/响应信封解析与事件流订阅**收敛在此**，
/// 壳层（CLI `ctrl`）不再手写 WS 客户端与 JSON `Value` 信封断言
/// （docs/layering-architecture.md：控制面协议契约与 [`CtrlServer`] 同层，
/// 壳层只做参数解析 + 展示）。
pub mod client {
    use std::time::Duration;

    use super::CtrlRequest;
    use anyhow::{Context, bail};
    use futures_util::{SinkExt, StreamExt};
    use serde_json::Value;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;

    /// 发送一个控制请求并等待响应（连接期间到达的 [`KernelEvent`] 帧忽略）。
    pub async fn request(connect: &str, req: CtrlRequest) -> anyhow::Result<Value> {
        let (mut ws, _) = connect_async(connect)
            .await
            .context("连接控制面失败（实例是否在运行？）")?;
        ws.send(Message::Text(serde_json::to_string(&req)?.into()))
            .await?;
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(text))) => {
                    let v: Value = serde_json::from_str(&text)?;
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

    /// 订阅控制面事件流（无 `rsp` 标签的 [`KernelEvent`]），收集 `secs` 秒。
    pub async fn collect_events(connect: &str, secs: u64) -> anyhow::Result<Vec<Value>> {
        let (mut ws, _) = connect_async(connect)
            .await
            .context("连接控制面失败（实例是否在运行？）")?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        let mut out = Vec::new();
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                msg = ws.next() => match msg {
                    Some(Ok(Message::Text(text))) => {
                        let v: Value = serde_json::from_str(&text)?;
                        // 事件无 rsp 标签；响应（rsp）忽略
                        if v.get("rsp").is_none() {
                            out.push(v);
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                },
            }
        }
        Ok(out)
    }
}
