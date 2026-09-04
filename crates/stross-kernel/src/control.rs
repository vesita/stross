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
use stross_endpoint::pipeline::StreamConfig;
use stross_proto::message::{Delivery, EndpointId, TransportPreference, Visibility};

use crate::Kernel;
use crate::SessionPrefs;
use crate::negotiator::ShareNegotiator;

/// 控制面默认端口（回环）；真源在 [`stross_types::ports`]，此处仅别名保持路径兼容。
pub use stross_types::ports::CTRL as DEFAULT_CTRL_PORT;

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
    /// 公开端点（端点框架，docs/endpoint-model-v2.md §2；P1 1:1）。
    EndpointPublish {
        endpoint_id: EndpointId,
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
    pub const fn ok(payload: serde_json::Value) -> Self {
        Self::Ok { payload }
    }
    pub fn err(message: impl Into<String>) -> Self {
        Self::Err {
            message: message.into(),
        }
    }

    /// 把类型化载荷序列化为 `Ok` 响应——wire 键由 serde 派生（单一真源），
    /// 不再手写 JSON 字符串键（docs/layering-architecture.md：壳层只消费
    /// 内核产出的类型，不自行定义响应结构）。
    fn ok_json<T: Serialize>(payload: T) -> Self {
        match serde_json::to_value(payload) {
            Ok(v) => Self::ok(v),
            Err(e) => Self::err(format!("载荷序列化失败: {e}")),
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
        let _ = self.task.await;
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
            CtrlResponse::ok_json(stross_types::PendingRequestsPayload {
                pending: neg.pending_requests(),
            })
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
                Ok(Some(grant)) => CtrlResponse::ok_json(stross_types::GrantResponseView {
                    stream_id: Some(grant.view.stream_id),
                    pin: Some(grant.view.pin),
                    expires_at: Some(grant.view.expires_at),
                    trusted: grant.trusted,
                    denied: false,
                }),
                Ok(None) => CtrlResponse::ok_json(stross_types::GrantResponseView {
                    stream_id: None,
                    pin: None,
                    expires_at: None,
                    trusted: false,
                    denied: true,
                }),
                Err(e) => CtrlResponse::err(e),
            }
        }
        CtrlRequest::EndpointPublish {
            endpoint_id,
            visibility,
            delivery,
            transports,
            codecs,
        } => match app.publish_endpoint(endpoint_id, visibility, delivery, transports, codecs) {
            Ok(m) => CtrlResponse::ok_json(m),
            Err(e) => CtrlResponse::err(e.to_user_string()),
        },
        CtrlRequest::EndpointPublishFile {
            path,
            visibility,
            delivery,
        } => match app.publish_file_endpoint(std::path::Path::new(&path), visibility, delivery) {
            Ok(m) => {
                let endpoint_id = EndpointId::new(m.kind, m.endpoint_id);
                CtrlResponse::ok_json(stross_types::FilePublishedView {
                    size: app.file_source(endpoint_id).map_or(0, |s| s.size),
                    endpoint_id,
                    name: m.name,
                    delivery: m.delivery,
                })
            }
            Err(e) => CtrlResponse::err(e.to_user_string()),
        },
        CtrlRequest::EndpointUnpublish { endpoint_id } => {
            let Some(endpoint_id) = EndpointId::parse(&endpoint_id) else {
                return CtrlResponse::err(format!("非法端点标识: {endpoint_id}"));
            };
            match app.unpublish_endpoint(endpoint_id).await {
                Ok(()) => CtrlResponse::ok_json(stross_types::UnpublishedView {
                    endpoint_id,
                    unpublished: true,
                }),
                Err(e) => CtrlResponse::err(e.to_user_string()),
            }
        }
        CtrlRequest::EndpointList => {
            // 单层端点模型：一张端点表（含未通告），published/available 自标注
            CtrlResponse::ok_json(stross_types::EndpointListPayload {
                endpoints: app.endpoint_catalog(),
            })
        }
        CtrlRequest::CreateSession { title, sinks } => {
            // 源节点固定为本机（register_local_node 注册的 "local"）；
            // title 随会话存储（UI 展示），不再是死字段
            let prefs = SessionPrefs {
                title,
                ..Default::default()
            };
            match app.create_session("local", &sinks, &prefs) {
                Ok(s) => CtrlResponse::ok_json(stross_types::SessionCreatedView {
                    session_id: s.id,
                    title: s.title,
                }),
                Err(e) => CtrlResponse::err(e.to_user_string()),
            }
        }
        CtrlRequest::Authorize {
            session_id,
            access_code,
        } => match app.authorize(&session_id, access_code.as_deref()) {
            Ok(()) => CtrlResponse::ok_json(stross_types::AuthorizedView {
                session_id: session_id.into(),
                authorized: true,
            }),
            Err(e) => CtrlResponse::err(e.to_user_string()),
        },
        CtrlRequest::Teardown { session_id } => match app.teardown(&session_id) {
            Ok(()) => CtrlResponse::ok_json(stross_types::TeardownView {
                session_id: session_id.into(),
            }),
            Err(e) => CtrlResponse::err(e.to_user_string()),
        },
        CtrlRequest::StartStream { config, relay_url } => {
            match app.start_stream(config, relay_url).await {
                Ok(r) => CtrlResponse::ok_json(r),
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
                Ok(token) => CtrlResponse::ok_json(stross_types::IssuedShareTokenView {
                    token: token.to_token_string(),
                    stream_id: token.stream_id,
                    pin: token.pin,
                    expires_at: token.expires_at,
                    media: token.media,
                }),
                Err(e) => CtrlResponse::err(e.to_user_string()),
            }
        }
        CtrlRequest::StopStream => match app.stop_stream().await {
            Ok(()) => CtrlResponse::ok_json(stross_types::StoppedView { stopped: true }),
            Err(e) => CtrlResponse::err(e.to_user_string()),
        },
        CtrlRequest::ListSessions => {
            let sessions: Vec<stross_types::SessionView> = app
                .sessions()
                .iter()
                .map(|s| stross_types::SessionView {
                    session_id: s.id.clone(),
                    source: s.source.clone(),
                    sinks: s.sinks.clone(),
                    requires_pin: s.requires_pin,
                })
                .collect();
            CtrlResponse::ok_json(stross_types::SessionsPayload { sessions })
        }
        CtrlRequest::Status => {
            let status = app.stream_status();
            let (ws_port, srt_port, quic_port) =
                app.relay_ports()
                    .unwrap_or((app.stream_relay_port(), None, None));
            CtrlResponse::ok_json(stross_types::StatusView {
                version: env!("CARGO_PKG_VERSION").into(),
                platform: app.platform_str().to_string(),
                uptime_secs: app.uptime_secs(),
                relay_port: ws_port,
                srt_port,
                quic_port,
                streaming: status.running,
                stream_id: status.stream_id,
                stream_title: status.title,
                stream_started_at: status.started_at,
                sessions: app.sessions().len(),
            })
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
                        // CtrlResponse::Err 变体序列化 tag 为 "err"（serde tag 用
                        // 变体名小写）；匹配实际 wire 值，否则错误响应被当事件忽略
                        // → 客户端无限等待（曾实测挂死）。
                        Some("err") => {
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

    /// 发送控制请求并**反序列化载荷为类型化视图**（[`crate::view`]）——
    /// 壳层不再手写 `payload["key"]` 字符串键（wire 键单一真源 = serde）。
    pub async fn request_as<T: serde::de::DeserializeOwned>(
        connect: &str,
        req: CtrlRequest,
    ) -> anyhow::Result<T> {
        let payload = request(connect, req).await?;
        serde_json::from_value(payload).context("控制响应载荷类型不符")
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
