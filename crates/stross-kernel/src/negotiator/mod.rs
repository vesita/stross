//! 凭证自动协商（权限自动化）：局域网设备免粘贴自动获取一次性接入凭证。
//!
//! # 动机
//!
//! B2 的手动路径需要用户把电脑端展示的 ShareToken 复制粘贴到手机上。本模块
//! 增加一个**凭证柜台端点**：手机直接 `POST /api/negotiator/request` 申请凭证，
//! 电脑端 GUI 首次人工确认（并可选记住设备），之后同设备自动签发，免粘贴。
//!
//! # 安全边界（与 B0「凭证式接入、零远程控制面暴露」一致）
//!
//! * 协商端点**只签发一次性短时凭证**（`create_share_token` 语义），不暴露任何
//!   控制操作（建会话 / 拆会话 / 启停推流仍在回环控制面 18778）
//! * **首次人工确认 + 信任记忆**：未知设备必须由本机用户显式允许；
//!   已信任设备自动签发（信任清单持久化在应用数据目录）
//! * 与手动粘贴凭证等价的风险面：局域网可信模型下，能看见屏幕/伪造请求的
//!   攻击者本就可拿证；凭证仍是短时一次性（默认 10 分钟），过期即失效
//!
//! # 结构
//!
//! * [`DeviceIdentity`]：本机持久化身份（device_id + device_name），首次运行生成
//! * [`TrustStore`]：信任设备清单（device_id → 名称 / 加入时间），JSON 持久化
//! * [`PendingRequest`]：等待人工确认的请求（推送给 [`NegotiatorUi`]）
//! * [`ShareNegotiator`]：axum 端点服务器 + 挂起请求管理

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use stross_proto::message::{
    derive_stream_id, Delivery, EndpointDir, EndpointManifest, EndpointNode, EndpointStrategy,
    MediaKind, TransportId, Visibility,
};
use stross_proto::time::unix_secs;
use tokio::sync::oneshot;

use crate::Kernel;
use crate::kernel::{SessionPrefs, SubscribeCtx};
use crate::lock::MutexExt;

mod api;
mod dto;

// 协商线协议类型（ShareRequest / ShareGrant / RelayAddr / ShareTokenView）已
// 收敛至 stross-proto::message::negotiator。此处经 dto 重导出保持既有路径
// `stross_kernel::ShareRequest` 等兼容。
pub use dto::{RelayAddr, ShareGrant, ShareRequest, ShareTokenView};

/// 协商端点默认端口（LAN 可达；防火墙需放行该 TCP 端口）。真源在
/// [`stross_types::ports`]（`NEGOTIATOR_DISCOVERY`：协商与发现权威同一端口）。
pub use stross_types::ports::NEGOTIATOR_DISCOVERY as DEFAULT_NEGOTIATOR_PORT;
/// 等待人工确认的超时（秒）。
const PENDING_TIMEOUT_SECS: u64 = 60;
/// 签发凭证默认有效期（秒）。
const DEFAULT_GRANT_TTL_SECS: u64 = 600;

// ---------------------------------------------------------------------------
// 身份与信任
// ---------------------------------------------------------------------------

/// 本机持久化身份（首次运行生成，之后稳定；device_id 是运行时生成标识）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceIdentity {
    pub device_id: String,
    pub device_name: String,
}

/// 已信任设备（信任清单条目）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustedDevice {
    pub name: String,
    pub added_at: u64,
}

/// 信任清单：`device_id → TrustedDevice`，JSON 持久化（应用数据目录）。
///
/// 桌面与 Android 共用：`base_dir` 由上层（Tauri `app_data_dir`）注入，
/// 本模块不依赖任何平台路径约定，可独立测试。
pub struct TrustStore {
    path: PathBuf,
    devices: Mutex<HashMap<String, TrustedDevice>>,
}

impl TrustStore {
    /// 从 `base_dir` 加载（不存在时视为空清单）。
    pub fn load(base_dir: &std::path::Path) -> Self {
        let path = base_dir.join("trusted_devices.json");
        let devices = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<HashMap<String, TrustedDevice>>(&s).ok())
            .unwrap_or_default();
        Self {
            path,
            devices: Mutex::new(devices),
        }
    }

    /// 是否已信任该设备。
    pub fn is_trusted(&self, device_id: &str) -> bool {
        self.devices.lock_poisoned().contains_key(device_id)
    }

    pub fn trusted_name(&self, device_id: &str) -> Option<String> {
        self.devices
            .lock_poisoned()
            .get(device_id)
            .map(|d| d.name.clone())
    }

    /// 记住设备（写入清单并持久化）。幂等：重复记住仅更新时间。
    pub fn remember(&self, device_id: &str, name: &str) {
        let mut map = self.devices.lock_poisoned();
        map.insert(
            device_id.to_string(),
            TrustedDevice {
                name: name.to_string(),
                added_at: unix_secs(),
            },
        );
        if let Some(parent) = self.path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            tracing::warn!("信任清单目录创建失败: {e}");
        }
        if let Err(e) = std::fs::write(
            &self.path,
            serde_json::to_string_pretty(&*map).unwrap_or_else(|_| "{}".into()),
        ) {
            tracing::warn!("信任清单持久化失败: {e}");
        }
    }

    /// 信任设备数量（事件/展示用）。
    pub fn len(&self) -> usize {
        self.devices.lock_poisoned().len()
    }

    /// 是否尚无信任设备。
    pub fn is_empty(&self) -> bool {
        self.devices.lock_poisoned().is_empty()
    }
}

/// 读取或生成本机身份。不存在时生成新的 `device_id` 并持久化。
///
/// name 为解析后的展示名（壳层经 `stross_bridge::device_name_or` 注入）；
/// 已有身份若 name 无标识意义（如 Android 早期写入的 `localhost`——
/// `/proc/sys/kernel/hostname`），用传入 name 覆盖，避免对端看到
/// 「localhost」这种节点名。
pub fn load_or_create_identity(base_dir: &std::path::Path, name: &str) -> DeviceIdentity {
    let path = base_dir.join("identity.json");
    if let Ok(s) = std::fs::read_to_string(&path)
        && let Ok(mut id) = serde_json::from_str::<DeviceIdentity>(&s)
    {
        // 已有身份若设备名无标识意义（如 Android 早期写入的 `localhost`，
        // `/proc/sys/kernel/hostname` 恒为 localhost），用传入展示名覆盖，
        // 避免对端看到「localhost」。
        if is_placeholder_name(&id.device_name) {
            id.device_name = name.to_string();
            let _ = std::fs::write(&path, serde_json::to_string_pretty(&id).unwrap_or_default());
        }
        return id;
    }
    let id = DeviceIdentity {
        device_id: new_device_id(),
        device_name: name.to_string(),
    };
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::warn!("身份目录创建失败: {e}");
    }
    if let Err(e) = std::fs::write(&path, serde_json::to_string_pretty(&id).unwrap_or_default()) {
        tracing::warn!("身份持久化失败: {e}");
    }
    id
}

/// 设备名是否无标识意义（空 / `localhost` / `android`——Android 主机名恒为
/// localhost，直接广播会得到无意义名字）。与 `stross_bridge::hostname` 的
/// placeholder 判定同语义；内联于 kernel（分层铁律：内核不依赖 stross-bridge）。
fn is_placeholder_name(name: &str) -> bool {
    let n = name.trim();
    n.is_empty() || n == "localhost" || n == "android"
}

/// 生成随机设备标识（16 字节 /dev/urandom → hex；失败时回退时间戳）。
fn new_device_id() -> String {
    let mut buf = [0u8; 16];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| {
            use std::io::Read;
            f.read_exact(&mut buf)
        })
        .is_ok();
    if ok {
        buf.iter().map(|b| format!("{b:02x}")).collect()
    } else {
        // 回退：时间戳 + 进程号（仅当 /dev/urandom 不可用，如受限沙箱）
        format!("dev-{}-{}", unix_secs(), std::process::id())
    }
}

// ---------------------------------------------------------------------------
// 待确认请求（UI 面，非线协议）
// ---------------------------------------------------------------------------

/// 待人工确认的请求（纯数据 DTO，定义收敛至 stross-types——应用契约层单一真源）。
pub use stross_types::PendingRequest;

/// UI 层回调接口：有挂起请求时通知（Tauri 层实现为 emit 事件）。
pub trait NegotiatorUi: Send + Sync {
    fn request_pending(&self, req: &PendingRequest);
}

/// 空实现：无 UI 时静默挂起直到超时（测试 / 无头环境）。
pub struct NoopUi;

impl NegotiatorUi for NoopUi {
    fn request_pending(&self, _req: &PendingRequest) {}
}

/// CLI 实现：把挂起请求打到日志，用户经 `stross ctrl negotiator-respond` 应答。
///
/// 无弹窗，但语义与 GUI 一致：请求挂起 60s，应答窗口内由控制面命令决定
/// 允许/拒绝；已信任设备仍自动签发（不受 UI 影响）。
pub struct CliUi;

impl NegotiatorUi for CliUi {
    fn request_pending(&self, req: &PendingRequest) {
        tracing::warn!(
            "设备 {}（{}）请求接入（{}），等待确认：stross ctrl negotiator-list / negotiator-respond {} --allow",
            req.device_name,
            req.device_id,
            req.media.join(","),
            req.id,
        );
    }
}

// ---------------------------------------------------------------------------
// 服务器
// ---------------------------------------------------------------------------

type PendingSender = oneshot::Sender<Result<ShareGrant, String>>;

/// 挂起请求条目（应答时按条目内容签发对应 grant）。
struct PendingEntry {
    device_id: String,
    device_name: String,
    /// 订阅目标端点（端点语义；旧语义为 `None`）。
    endpoint_id: Option<String>,
    /// 订阅方选定的策略 id（注册表第三层；`None` = 端点默认策略）。
    strategy_id: Option<String>,
    /// 订阅方期望的 delivery。
    delivery_mode: Option<Delivery>,
    /// push 模式：订阅方中继 HTTP 基址 + 自签凭证（授予成功后触发驱动）。
    relay_addr: Option<String>,
    share_token: Option<String>,
    tx: PendingSender,
}

/// 挂起请求表：req_id → 条目。
type PendingMap = Arc<Mutex<HashMap<String, PendingEntry>>>;

/// 凭证协商服务器。
pub struct ShareNegotiator {
    app: Arc<Kernel>,
    store: Arc<TrustStore>,
    pending: PendingMap,
    task: tokio::task::JoinHandle<()>,
    /// 实际监听端口（`port = 0` 时随机）。
    pub port: u16,
}

impl ShareNegotiator {
    /// 启动协商端点（绑定 `0.0.0.0`；`port = 0` = 随机端口）。
    ///
    /// `base_dir`：身份 / 信任清单持久化目录（Tauri `app_data_dir`）。
    pub async fn start(
        app: Arc<Kernel>,
        ui: Arc<dyn NegotiatorUi>,
        base_dir: &std::path::Path,
        port: u16,
    ) -> anyhow::Result<Self> {
        let state = Arc::new(ServerState {
            app,
            store: Arc::new(TrustStore::load(base_dir)),
            ui,
            pending: Arc::new(Mutex::new(HashMap::new())),
        });
        // 路由 + OpenAPI 在 api 子模块（cors_layer 亦移入）。
        let router = api::router(state.clone());
        let addr = SocketAddr::from(([0, 0, 0, 0], port));
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| anyhow::anyhow!("绑定协商端口失败: {e}"))?;
        let actual = listener.local_addr()?.port();
        let task = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, router).await {
                tracing::error!("协商服务退出: {e}");
            }
        });
        tracing::info!("凭证协商端点已启动: 0.0.0.0:{actual}/api/negotiator/request");
        Ok(Self {
            app: state.app.clone(),
            store: state.store.clone(),
            pending: state.pending.clone(),
            task,
            port: actual,
        })
    }

    /// 应答挂起请求（GUI 用户在确认弹窗操作后调用）。
    ///
    /// * `allow = true`：签发凭证并返回给申请方；`remember` 额外写入信任清单
    /// * `allow = false`：拒绝（申请方收到 403）
    ///
    /// 返回 `Ok(Some(grant))` = 允许并已签发（GUI 用 streamId 启动自动接收）；
    /// `Ok(None)` = 拒绝；`Err` = 请求不存在 / 已处理等内部错误。
    pub fn respond(
        &self,
        req_id: &str,
        allow: bool,
        remember: bool,
    ) -> Result<Option<ShareGrant>, String> {
        let entry = self
            .pending
            .lock_poisoned()
            .remove(req_id)
            .ok_or_else(|| format!("挂起请求不存在或已处理: {req_id}"))?;
        if allow {
            // 先写信任再签发：grant.trusted 反映"该设备本次已受信任"
            if remember {
                self.store.remember(&entry.device_id, &entry.device_name);
            }
            let grant = self.grant(
                entry.device_id.clone(),
                entry.device_name.clone(),
                entry.endpoint_id.clone(),
                entry.strategy_id.clone(),
                entry.delivery_mode,
            )?;
            // 订阅达成：触发上层驱动（文件泵 / 媒体自动推流），docs §5 联动
            self.notify_subscribed(
                entry.endpoint_id.as_deref(),
                &grant,
                &entry.device_id,
                entry.relay_addr.as_deref(),
                entry.share_token.as_deref(),
            );
            let _ = entry.tx.send(Ok(grant.clone()));
            Ok(Some(grant))
        } else {
            let _ = entry.tx.send(Err("用户拒绝".into()));
            Ok(None)
        }
    }

    /// 绝对不可达的请求计数（诊断用）。
    pub fn pending_len(&self) -> usize {
        self.pending.lock_poisoned().len()
    }

    /// 挂起请求只读视图（CLI 控制面「列挂起」用；应答通道不可见）。
    pub fn pending_requests(&self) -> Vec<PendingRequest> {
        self.pending
            .lock_poisoned()
            .iter()
            .map(|(id, e)| PendingRequest {
                id: id.clone(),
                device_id: e.device_id.clone(),
                device_name: e.device_name.clone(),
                media: vec!["mic".into()], // 旧语义固定 mic；端点语义走 endpoint_name
                endpoint_name: e
                    .endpoint_id
                    .as_ref()
                    .and_then(|eid| self.app.endpoint_manifest(eid))
                    .map(|m| m.name.clone()),
                created_at: 0, // 挂起表未记录创建时刻，置 0 表示未知
            })
            .collect()
    }

    fn grant(
        &self,
        device_id: String,
        device_name: String,
        endpoint_id: Option<String>,
        strategy_id: Option<String>,
        delivery_mode: Option<Delivery>,
    ) -> Result<ShareGrant, String> {
        let endpoint = endpoint_id
            .as_ref()
            .and_then(|eid| self.app.endpoint_manifest(eid));
        let (title, media) = match &endpoint {
            Some(m) => (format!("接收 {} 共享", m.name), vec![m.kind]),
            None => (format!("接收 {device_name} 共享"), vec![MediaKind::Mic]),
        };
        compose_grant(
            &self.app,
            &self.store,
            &device_id,
            endpoint.as_ref(),
            strategy_id.as_deref(),
            delivery_mode,
            media,
            title,
        )
    }

    /// 订阅达成事件：构造 [`SubscribeCtx`] 触发公开方驱动开推
    /// （docs/endpoint-model-v2.md §4 联动；只对端点语义生效）。
    ///
    /// * pull：数据面流 id = 公开方本机会话（`grant.view.stream_id`），
    ///   推入自己的受控中继，无需凭证；
    /// * push：流 id / 凭证取自**订阅方**自签 token（订阅方中继校验用）。
    fn notify_subscribed(
        &self,
        endpoint_id: Option<&str>,
        grant: &ShareGrant,
        subscriber: &str,
        relay_addr: Option<&str>,
        share_token: Option<&str>,
    ) {
        notify_subscribed(
            &self.app,
            endpoint_id,
            grant,
            subscriber,
            relay_addr,
            share_token,
        );
    }

    /// 停止协商服务。
    pub async fn stop(self) {
        self.task.abort();
    }
}

/// 服务器共享状态（与 [`CtrlServer`] 的 `CtrlState` 同构）。
pub(crate) struct ServerState {
    app: Arc<Kernel>,
    store: Arc<TrustStore>,
    ui: Arc<dyn NegotiatorUi>,
    pending: PendingMap,
}

#[utoipa::path(
    post,
    path = "/api/negotiator/request",
    tag = "negotiator",
    request_body = ShareRequest,
    responses(
        (status = 200, description = "凭证已签发（或已合并到现有活动共享）", body = ShareGrant),
        (status = 400, description = "media 为空", body = dto::ApiError),
        (status = 403, description = "被拒绝（不在白名单 / 用户拒绝 / 等待确认超时）", body = dto::ApiError),
        (status = 404, description = "端点不存在或不可挂载", body = dto::ApiError),
        (status = 500, description = "内部错误", body = dto::ApiError),
        (status = 504, description = "等待用户确认超时", body = dto::ApiError)
    )
)]
pub(crate) async fn handle_request(
    State(state): State<Arc<ServerState>>,
    Json(req): Json<ShareRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    // 端点语义：先校验端点存在与可挂载（404）；旧语义校验 media 非空（400）
    let endpoint = match &req.endpoint_id {
        Some(eid) => match state.app.endpoint_manifest(eid) {
            Some(m) if !m.available => {
                let reason = m.last_error.as_deref().unwrap_or("未知原因");
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({
                        "error": format!("端点不可用（{reason}）: {eid}")
                    })),
                );
            }
            Some(m) => Some(m),
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error": "端点不存在或已取消公开" })),
                );
            }
        },
        None => None,
    };
    if endpoint.is_none() && req.media.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "media 不能为空" })),
        );
    }
    // 端点语义：媒体名取目标端点 kind（端点框架请求 `media` 为空，见
    // subscriber::request_endpoint_grant）；旧语义（无 endpoint_id）取请求方
    // media 列表。二者都序列化为 MediaKind camelCase 名（前端按此映射中文标签，
    // 否则「想订阅你共享的内容」会显示「未知媒体」）。
    let media_source: Vec<MediaKind> = match &endpoint {
        Some(m) => vec![m.kind],
        None => req.media.clone(),
    };
    let media_names: Vec<String> = media_source
        .iter()
        .map(|m| {
            serde_json::to_string(m)
                .unwrap_or_default()
                .trim_matches('"')
                .to_string()
        })
        .collect();

    // 按可见性决策（端点语义）或旧信任语义（docs/endpoint-model-v2.md §4）
    match policy_decision(&state.store, endpoint.as_ref(), &req.device_id) {
        Decision::Grant => {
            let (title, media) = match &endpoint {
                Some(m) => (format!("接收 {} 共享", m.name), vec![m.kind]),
                None => (format!("接收 {} 共享", req.device_name), req.media.clone()),
            };
            match compose_grant(
                &state.app,
                &state.store,
                &req.device_id,
                endpoint.as_ref(),
                req.strategy_id.as_deref(),
                req.delivery_mode,
                media,
                title,
            ) {
                Ok(grant) => {
                    // 订阅达成：触发上层驱动（docs §5 联动）
                    notify_subscribed(
                        &state.app,
                        endpoint.as_ref().map(|m| m.endpoint_id.as_str()),
                        &grant,
                        &req.device_id,
                        req.relay_addr.as_deref(),
                        req.share_token.as_deref(),
                    );
                    (
                        StatusCode::OK,
                        Json(serde_json::to_value(grant).unwrap_or_default()),
                    )
                }
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": e })),
                ),
            }
        }
        Decision::Reject(msg) => (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": msg })),
        ),
        Decision::Pending => {
            // 未知设备 / 首见 Confirm → 挂起等待人工确认（60s 超时）
            let id = format!("n{}", new_pending_id());
            let (tx, rx) = oneshot::channel();
            {
                let mut pending = state.pending.lock_poisoned();
                pending.insert(
                    id.clone(),
                    PendingEntry {
                        device_id: req.device_id.clone(),
                        device_name: req.device_name.clone(),
                        endpoint_id: req.endpoint_id.clone(),
                        strategy_id: req.strategy_id.clone(),
                        delivery_mode: req.delivery_mode,
                        relay_addr: req.relay_addr.clone(),
                        share_token: req.share_token.clone(),
                        tx,
                    },
                );
            }
            state.ui.request_pending(&PendingRequest {
                id: id.clone(),
                device_id: req.device_id.clone(),
                device_name: req.device_name.clone(),
                media: media_names,
                endpoint_name: endpoint.as_ref().map(|m| m.name.clone()),
                created_at: unix_secs(),
            });

            match tokio::time::timeout(Duration::from_secs(PENDING_TIMEOUT_SECS), rx).await {
                Ok(Ok(Ok(grant))) => (
                    StatusCode::OK,
                    Json(serde_json::to_value(grant).unwrap_or_default()),
                ),
                Ok(Ok(Err(e))) => (
                    StatusCode::FORBIDDEN,
                    Json(serde_json::json!({ "error": e })),
                ),
                // 超时：清除挂起（防止泄漏 oneshot / 阻止迟到应答）
                Err(_) => {
                    let _ = state.pending.lock_poisoned().remove(&id);
                    (
                        StatusCode::GATEWAY_TIMEOUT,
                        Json(serde_json::json!({ "error": "等待用户确认超时" })),
                    )
                }
                Ok(Err(_)) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": "内部错误" })),
                ),
            }
        }
    }
}

/// 端点订阅决策：按可见性与信任关系决定 自动签发 / 挂起人工确认 / 拒绝。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Grant,
    Pending,
    Reject(&'static str),
}

/// 可见性决策表（docs/endpoint-model-v2.md §4）：
/// Public 免确认；Confirm 已信任自动、未信任挂起；Private 白名单自动、否则拒绝；
/// 无端点（旧语义）= 信任自动、未信任挂起。
fn policy_decision(
    store: &TrustStore,
    endpoint: Option<&EndpointManifest>,
    requester: &str,
) -> Decision {
    match endpoint {
        None => {
            if store.is_trusted(requester) {
                Decision::Grant
            } else {
                Decision::Pending
            }
        }
        Some(m) => match &m.visibility {
            Visibility::Public => Decision::Grant,
            Visibility::Confirm => {
                if store.is_trusted(requester) {
                    Decision::Grant
                } else {
                    Decision::Pending
                }
            }
            Visibility::Private { nodes } => {
                if nodes.iter().any(|n| n == requester) {
                    Decision::Grant
                } else {
                    Decision::Reject("请求方不在该端点的白名单内")
                }
            }
        },
    }
}

/// 统一签发 grant（旧语义与端点语义共用）。
///
/// 端点语义：公开方拍板 delivery（`Both` 时尊重订阅方期望、缺省 Pull），
/// 传输列表按公开者声明的优先序透传，pull 模式附带本机中继地址。
///
/// **订阅收敛（iteration-plan.md 第十二轮）**：同端点已有活动共享时——
/// * pull 复用同一流（中继多 watcher 生效，不新建会话/凭证、不重复触发 share）；
/// * push 拒绝（公开方单引擎，一次仅一个订阅者，避免"grant 成功但流永不存在"）。
#[allow(clippy::too_many_arguments)] // 签发原语一次性组合协商全部参数，保持扁平
fn compose_grant(
    app: &Kernel,
    store: &TrustStore,
    device_id: &str,
    endpoint: Option<&EndpointManifest>,
    strategy_id: Option<&str>,
    delivery_mode: Option<Delivery>,
    media: Vec<MediaKind>,
    title: String,
) -> Result<ShareGrant, String> {
    // 订阅收敛检查（先于建会话：复用不产生新会话）
    if let Some(m) = endpoint
        && let Some((sid, _)) = app.active_share_by_endpoint(&m.endpoint_id)
    {
        // 订阅驱动（docs/endpoint-model-v2.md §4 定稿）：只在 pull 复用——
        // 同端点已有活动共享（只走 pull），订阅方只用 stream_id（watch 路径）
        // 复用同一流，凭证/中继地址同现流。
        tracing::info!(
            "端点「{}」已有活动共享（{sid}），订阅方 {device_id} 复用同一流",
            m.name
        );
        return Ok(ShareGrant {
            view: stross_types::ShareTokenView {
                token: String::new(),
                stream_id: sid,
                pin: String::new(),
                expires_at: 0,
            },
            trusted: store.is_trusted(device_id),
            delivery: Some(Delivery::Pull),
            transports: Some(m.transports.iter().map(|t| t.transport).collect()),
            transport_profile: Some(m.transport_profile),
            pick_rule: Some(m.pick_rule),
            strategy: Some(checked_strategy(m, strategy_id)?),
            relay: app.relay_ports().map(|(ws, srt, quic)| RelayAddr {
                ws_port: ws,
                srt_port: srt,
                quic_port: quic,
            }),
        });
    }
    // 数据面流 id 来源（订阅驱动 pull，docs/comm-mode-v2.md §6「配套改动」）：
    // * 端点语义 → **语义 id 派生**：`derive(endpoint_id, transport_profile,
    //   pick_rule)` 确定性三要素——订阅方本地可推导、同端点收敛同流、
    //   停一路不级联；pull 不需要凭证，token 为空；
    // * 旧语义（无端点，B2 凭证式接入如「接收手机麦克风」）→ 内核签发
    //   sess-N + 一次性凭证（保持现状）。
    let view = match endpoint {
        None => app
            .issue_share_token_for(title, media, Some(DEFAULT_GRANT_TTL_SECS))
            .map_err(|e| e.to_user_string())?,
        Some(m) => {
            let sid = derive_stream_id(&m.endpoint_id, m.transport_profile, m.pick_rule);
            app.ensure_session_with_id(
                &sid,
                "local",
                &["local".into()],
                &SessionPrefs {
                    title,
                    ..Default::default()
                },
            )
            .map_err(|e| e.to_user_string())?;
            ShareTokenView {
                token: String::new(),
                stream_id: sid,
                pin: String::new(),
                expires_at: 0,
            }
        }
    };
    // 订阅驱动（docs/endpoint-model-v2.md §4 定稿）：数据流一律由订阅方发起并
    // 主动取（pull），共享方只在本地中继发布、不做任何主动出站推送。delivery
    // 定稿恒为 Pull（保留枚举 wire 兼容——对端旧版本字段仍可解析，但本端
    // 协商不再产出 push/both 路径）。
    let _ = delivery_mode;
    let (delivery, transports, relay) = match endpoint {
        None => (None, None, None),
        Some(m) => {
            let delivery = Delivery::Pull;
            let transports: Vec<TransportId> = m.transports.iter().map(|t| t.transport).collect();
            let relay = app.relay_ports().map(|(ws, srt, quic)| RelayAddr {
                ws_port: ws,
                srt_port: srt,
                quic_port: quic,
            });
            (Some(delivery), Some(transports), relay)
        }
    };
    Ok(ShareGrant {
        view,
        trusted: store.is_trusted(device_id),
        delivery,
        transports,
        transport_profile: endpoint.map(|m| m.transport_profile),
        pick_rule: endpoint.map(|m| m.pick_rule),
        strategy: endpoint
            .map(|m| checked_strategy(m, strategy_id))
            .transpose()?,
        relay,
    })
}

/// 策略 → 定稿（按订阅方选定的策略 id 精确取，缺省默认策略）并**校验内核
/// 序列化工具支持**：未实现的序列化规则（如预留的 Chunked 分包）拒绝授予，
/// 不静默降级——数据契约在协商边界就锁定（docs/endpoint-model-v2.md §0）。
fn checked_strategy(
    m: &EndpointManifest,
    strategy_id: Option<&str>,
) -> Result<EndpointStrategy, String> {
    let strategy = strategy_of(m, strategy_id);
    if crate::pick::loader_for(&strategy).is_none() {
        return Err(format!(
            "内核不支持序列化规则 {:?}（端点 {} 策略 {}）——协商拒绝，不静默降级",
            strategy.serialize, m.endpoint_id, strategy.strategy_id
        ));
    }
    Ok(strategy)
}

/// 清单 → 定稿策略组合（注册表第三层；按订阅方选定的策略 id 精确取，
/// 缺省 = 默认策略（首个），再由平铺 `pick_rule` 推导直通 + pick 兜底——
/// 与 [`crate::kernel::endpoint`] 的策略推导同语义）。
fn strategy_of(m: &EndpointManifest, strategy_id: Option<&str>) -> EndpointStrategy {
    m.strategies
        .iter()
        .find(|s| Some(s.strategy_id.as_str()) == strategy_id)
        .cloned()
        .or_else(|| m.strategies.first().cloned())
        .unwrap_or_else(|| EndpointStrategy {
            strategy_id: EndpointStrategy::DEFAULT_ID.into(),
            serialize: stross_proto::message::SerializeRule::Passthrough,
            pick: m.pick_rule,
        })
}

/// 订阅达成事件（自由函数版，`handle_request` / `respond` 共用）：构造
/// [`SubscribeCtx`] 触发端点 `share` 自动开推（docs/endpoint-model-v2.md §3
/// 契约 / §4 数据流联动；只对端点语义生效）。
///
/// 订阅驱动（docs/endpoint-model-v2.md §4 定稿）：只走 pull——数据面流 id =
/// 公开方本机会话（`grant.view.stream_id`），推入自己的受控中继，无需凭证；
/// 订阅方连公开方中继 watch 取流（无 push 出站路径）。
fn notify_subscribed(
    app: &Arc<Kernel>,
    endpoint_id: Option<&str>,
    grant: &ShareGrant,
    subscriber: &str,
    _relay_addr: Option<&str>,
    _share_token: Option<&str>,
) {
    let Some(endpoint_id) = endpoint_id else {
        return; // 旧语义（无端点）不触发联动
    };
    let Some(delivery) = grant.delivery else {
        return;
    };
    // 订阅收敛（iteration-plan.md 第十二轮）：该端点已有活动共享（复用场景）
    // → 不重复触发 share（流已在推，新订阅者直接 watch 同流）
    if app.active_share_by_endpoint(endpoint_id).is_some() {
        tracing::info!("端点 {endpoint_id} 已有活动共享，复用流（订阅方 {subscriber}）");
        return;
    }
    let ctx = SubscribeCtx {
        subscriber: subscriber.to_string(),
        delivery,
        stream_id: grant.view.stream_id.clone(),
        transport_profile: grant.transport_profile.unwrap_or_default(),
        strategy: grant.strategy.clone().unwrap_or_else(|| EndpointStrategy {
            strategy_id: EndpointStrategy::DEFAULT_ID.into(),
            serialize: stross_proto::message::SerializeRule::Passthrough,
            pick: grant.pick_rule.unwrap_or_default(),
        }),
        relay_addr: None,
        share_token: None,
    };
    app.on_endpoint_subscribed(app.clone(), endpoint_id, &ctx);
}

/// 目录 API（L2）：本节点**已通告**端点（不可挂载端点可见但不可订阅——
/// `available=false` 由订阅方 UI 与握手校验拒绝；Private 端点不对目录公开，§9）。
#[utoipa::path(
    get,
    path = "/api/endpoints",
    tag = "negotiator",
    responses((status = 200, description = "本节点已通告端点目录", body = EndpointDir))
)]
pub(crate) async fn handle_endpoints(State(state): State<Arc<ServerState>>) -> Json<EndpointDir> {
    let endpoints: Vec<EndpointManifest> = state
        .app
        .published_endpoints()
        .into_iter()
        .filter(|e| !matches!(e.visibility, Visibility::Private { .. }))
        .collect();
    let (device_id, device_name) = state.app.device_identity().map_or_else(
        || ("".into(), "本机".into()),
        |i| (i.device_id, i.device_name),
    );
    // 类型化构造（stross-proto EndpointDir）：序列化与旧 json! 逐字节一致
    // （node/deviceId/deviceName、endpoints，全 camelCase）
    let dir = EndpointDir {
        node: EndpointNode {
            device_id,
            device_name,
        },
        endpoints,
    };
    Json(dir)
}

/// 统一发现清单（`GET /api/discovery`）：本节点权威节点信息（身份 + 能力 +
/// 中继入口端口），由 [`crate::Kernel::discovery_manifest`] 组装。mDNS 与
/// 子网扫描都据此收敛到**同一台设备同一个 `relay_port`**，降低用户认知成本。
#[utoipa::path(
    get,
    path = "/api/discovery",
    tag = "negotiator",
    responses(
        (status = 200, description = "本节点统一发现清单（身份+能力+中继入口端口）", body = crate::discovery::DiscoveryResp),
        (status = 404, description = "本节点未锚定（无中继入口，非可发现节点）", body = dto::ApiError)
    )
)]
pub(crate) async fn handle_discovery(
    State(state): State<Arc<ServerState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.app.discovery_manifest() {
        Some(m) => (
            StatusCode::OK,
            Json(serde_json::to_value(m).unwrap_or_default()),
        ),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "本节点未锚定中继" })),
        ),
    }
}

fn new_pending_id() -> u64 {
    // 全局自增：时间戳 + 进程内计数器（多实例互不冲突）
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Endpoint, EndpointBase, MicEndpoint, Platform, Probe, ScreenEndpoint, ShareEndpoint,
        SystemAudioEndpoint,
    };
    use stross_proto::message::MediaKind;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("stross-neg-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 探测恒成功（测试环境无真实采集源，只验证契约）。
    fn ok_probe() -> Probe {
        std::sync::Arc::new(|| Ok(()))
    }

    /// 桌面平台端点 fixture（平台端点构造收在 stross-bridge；测试自备等价清单）。
    fn desktop_kernel() -> Kernel {
        let k = Kernel::new(Platform::Desktop);
        k.seed_endpoint(Box::new(ScreenEndpoint::new("屏幕", ok_probe())));
        k.seed_endpoint(Box::new(MicEndpoint::new("麦克风", ok_probe())));
        k.seed_endpoint(Box::new(SystemAudioEndpoint::new("系统声音", ok_probe())));
        k
    }

    #[test]
    fn identity_persists_and_stable() {
        let dir = tmp_dir("identity");
        let a = load_or_create_identity(&dir, "电脑");
        let b = load_or_create_identity(&dir, "电脑");
        assert_eq!(a.device_id, b.device_id, "同一目录下身份必须稳定");
        assert_eq!(a.device_name, b.device_name);
        assert!(!a.device_id.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn trust_store_persists() {
        let dir = tmp_dir("trust");
        {
            let s = TrustStore::load(&dir);
            assert!(!s.is_trusted("dev-x"));
            s.remember("dev-x", "手机A");
            assert!(s.is_trusted("dev-x"));
            assert_eq!(s.trusted_name("dev-x").as_deref(), Some("手机A"));
        }
        // 重新加载（模拟重启）：持久化生效
        {
            let s = TrustStore::load(&dir);
            assert!(s.is_trusted("dev-x"), "重启后信任应保留");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_device_id_unique_and_hex() {
        let a = new_device_id();
        let b = new_device_id();
        assert_ne!(a, b);
        assert!(a.len() >= 16);
    }

    #[tokio::test]
    async fn unknown_device_pends_and_allow_grants() {
        let dir = tmp_dir("http");
        let app = Arc::new(desktop_kernel());
        // 直接驱动挂起机制（不走 HTTP，避免端口/运行时依赖）：
        // 1) 未知设备请求 → 挂起表登记（真实路径中同时触发 NegotiatorUi::request_pending）
        let neg = ShareNegotiator {
            app: app.clone(),
            store: Arc::new(TrustStore::load(&dir)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            task: tokio::spawn(async {}),
            port: 0,
        };
        assert!(!neg.store.is_trusted("dev-phone-1"), "未知设备未信任");
        let (tx2, rx2) = oneshot::channel();
        neg.pending.lock_poisoned().insert(
            "n1".into(),
            PendingEntry {
                device_id: "dev-phone-1".into(),
                device_name: "手机A".into(),
                endpoint_id: None,
                strategy_id: None,
                delivery_mode: None,
                relay_addr: None,
                share_token: None,
                tx: tx2,
            },
        );

        // 2) 用户应答：允许 + 记住
        neg.respond("n1", true, true).unwrap();
        let grant = rx2.await.unwrap().unwrap();
        assert!(!grant.view.token.is_empty());
        assert!(!grant.view.stream_id.is_empty());
        assert!(grant.view.expires_at > unix_secs());
        assert!(neg.store.is_trusted("dev-phone-1"), "允许时应记住设备");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn trusted_device_auto_grants() {
        let dir = tmp_dir("trusted");
        let store = TrustStore::load(&dir);
        store.remember("dev-phone-2", "手机B");
        let app = Arc::new(desktop_kernel());
        let neg = ShareNegotiator {
            app,
            store: Arc::new(store),
            pending: Arc::new(Mutex::new(HashMap::new())),
            task: tokio::spawn(async {}),
            port: 0,
        };
        let grant = neg
            .grant("dev-phone-2".into(), "手机B".into(), None, None, None)
            .expect("信任设备应自动签发");
        assert!(grant.trusted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn deny_returns_error() {
        let dir = tmp_dir("deny");
        let app = Arc::new(desktop_kernel());
        let neg = ShareNegotiator {
            app,
            store: Arc::new(TrustStore::load(&dir)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            task: tokio::spawn(async {}),
            port: 0,
        };
        let (tx, rx) = oneshot::channel();
        neg.pending.lock_poisoned().insert(
            "n3".into(),
            PendingEntry {
                device_id: "dev-3".into(),
                device_name: "手机C".into(),
                endpoint_id: None,
                strategy_id: None,
                delivery_mode: None,
                relay_addr: None,
                share_token: None,
                tx,
            },
        );
        neg.respond("n3", false, false).unwrap();
        let res = rx.await.unwrap();
        assert!(res.is_err(), "拒绝时应返回错误给申请方");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn endpoint_public_grant_carries_delivery_and_transports() {
        let dir = tmp_dir("epub");
        let app = Arc::new(desktop_kernel());
        app.publish_endpoint(
            "mic:builtin",
            Visibility::Public,
            Delivery::Pull,
            None,
            None,
        )
        .expect("公开麦克风端点");
        let neg = ShareNegotiator {
            app: app.clone(),
            store: Arc::new(TrustStore::load(&dir)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            task: tokio::spawn(async {}),
            port: 0,
        };
        let grant = neg
            .grant(
                "dev-phone".into(),
                "手机A".into(),
                Some("mic:builtin".into()),
                None,
                None,
            )
            .expect("Public 端点应自动签发");
        assert_eq!(grant.delivery, Some(Delivery::Pull));
        let transports = grant.transports.expect("应携带传输列表");
        assert_eq!(transports[0], TransportId::Quic, "公开者默认协议 QUIC 优先");
        // 本测试未启动中继 → pull 模式 relay 为 None（无地址可给）
        assert!(grant.relay.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn endpoint_delivery_both_granted_as_pull_subscription_driven() {
        let dir = tmp_dir("both");
        let app = Arc::new(desktop_kernel());
        app.publish_endpoint(
            "sysaudio:builtin",
            Visibility::Public,
            Delivery::Both,
            None,
            None,
        )
        .expect("公开系统声音端点");
        let neg = ShareNegotiator {
            app: app.clone(),
            store: Arc::new(TrustStore::load(&dir)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            task: tokio::spawn(async {}),
            port: 0,
        };
        // 订阅驱动定稿：无论端点声明的 Both，协商只产出 Pull（无 push 路径）；
        // 订阅方指明 Push 也被收敛为 Pull。
        let grant = neg
            .grant(
                "dev-phone".into(),
                "手机A".into(),
                Some("sysaudio:builtin".into()),
                None,
                Some(Delivery::Push),
            )
            .unwrap();
        assert_eq!(grant.delivery, Some(Delivery::Pull), "订阅驱动只走 pull");
        // Both + 未指明 → 仍 Pull
        let grant = neg
            .grant(
                "dev-phone".into(),
                "手机A".into(),
                Some("sysaudio:builtin".into()),
                None,
                None,
            )
            .unwrap();
        assert_eq!(grant.delivery, Some(Delivery::Pull));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 订阅收敛（iteration-plan.md 第十二轮）：同端点已有活动共享（pull）时，
    /// 第二个订阅者复用同一流（不新建会话/凭证），grant.stream_id 与首个一致。
    #[tokio::test]
    async fn pull_reuse_same_stream_for_second_subscriber() {
        let dir = tmp_dir("reuse");
        let app = Arc::new(desktop_kernel());
        app.publish_endpoint(
            "mic:builtin",
            Visibility::Public,
            Delivery::Pull,
            None,
            None,
        )
        .expect("公开麦克风端点");
        let neg = ShareNegotiator {
            app: app.clone(),
            store: Arc::new(TrustStore::load(&dir)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            task: tokio::spawn(async {}),
            port: 0,
        };
        // 第一个订阅者：无活动共享 → 新建会话
        let g1 = neg
            .grant(
                "dev-a".into(),
                "设备A".into(),
                Some("mic:builtin".into()),
                None,
                None,
            )
            .unwrap();
        let sid1 = g1.view.stream_id.clone();
        assert!(!sid1.is_empty());
        // 模拟端点共享已登记（真实路径：share → start_stream 成功 → note_share_active）
        let weak: std::sync::Weak<dyn crate::EndpointApp> =
            std::sync::Arc::downgrade(&(app.clone() as std::sync::Arc<dyn crate::EndpointApp>));
        app.note_share_active(weak, "mic:builtin", &sid1, Delivery::Pull);
        // 第二个订阅者：复用同一流
        let g2 = neg
            .grant(
                "dev-b".into(),
                "设备B".into(),
                Some("mic:builtin".into()),
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            g2.view.stream_id, sid1,
            "pull 复用：第二个订阅者拿同一流 id"
        );
        assert_eq!(g2.delivery, Some(Delivery::Pull));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 语义 id 派生（docs/comm-mode-v2.md §6）：端点订阅的 grant 流 id =
    /// `derive(endpoint_id, transport_profile, pick_rule)` 确定性三要素——
    /// 同端点必然同 id（结构性订阅收敛），且该 id 已建内核会话
    /// （受控中继预授权接入的基础）。
    #[tokio::test]
    async fn endpoint_grant_stream_id_is_derived_and_session_exists() {
        let dir = tmp_dir("derived");
        let app = Arc::new(desktop_kernel());
        app.publish_endpoint(
            "mic:builtin",
            Visibility::Public,
            Delivery::Pull,
            None,
            None,
        )
        .expect("公开麦克风端点");
        let neg = ShareNegotiator {
            app: app.clone(),
            store: Arc::new(TrustStore::load(&dir)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            task: tokio::spawn(async {}),
            port: 0,
        };
        let g1 = neg
            .grant(
                "dev-a".into(),
                "设备A".into(),
                Some("mic:builtin".into()),
                None,
                None,
            )
            .unwrap();
        let m = app.endpoint_manifest("mic:builtin").unwrap();
        let expected = derive_stream_id(&m.endpoint_id, m.transport_profile, m.pick_rule);
        assert_eq!(
            g1.view.stream_id, expected,
            "端点订阅流 id = 语义派生 id（{}）",
            expected
        );
        assert!(g1.view.token.is_empty(), "订阅驱动 pull 不需要凭证");
        assert!(
            app.has_session(&expected),
            "派生 id 已建内核会话（受控中继可预授权接入）"
        );
        // 无活动共享时再次 grant → 同 id（确定性派生 + 会话幂等，不产生新会话）
        let g2 = neg
            .grant(
                "dev-b".into(),
                "设备B".into(),
                Some("mic:builtin".into()),
                None,
                None,
            )
            .unwrap();
        assert_eq!(g2.view.stream_id, expected, "确定性派生：同端点同 id");
        assert_eq!(app.sessions().len(), 1, "会话幂等：派生 id 不重复建会话");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 订阅收敛：同端点已有活动共享时，第二个订阅者复用同一流（订阅驱动
    /// 只走 pull——即使端点声明 Push 也被收敛为 Pull 并复用）。
    #[tokio::test]
    async fn push_declared_endpoint_still_reuses_as_pull() {
        let dir = tmp_dir("pushrej");
        let app = Arc::new(desktop_kernel());
        app.publish_endpoint(
            "mic:builtin",
            Visibility::Public,
            Delivery::Push,
            None,
            None,
        )
        .expect("公开麦克风端点");
        let neg = ShareNegotiator {
            app: app.clone(),
            store: Arc::new(TrustStore::load(&dir)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            task: tokio::spawn(async {}),
            port: 0,
        };
        let g1 = neg
            .grant(
                "dev-a".into(),
                "设备A".into(),
                Some("mic:builtin".into()),
                None,
                Some(Delivery::Push),
            )
            .unwrap();
        assert_eq!(g1.delivery, Some(Delivery::Pull), "订阅驱动收敛为 pull");
        let sid1 = g1.view.stream_id.clone();
        let weak: std::sync::Weak<dyn crate::EndpointApp> =
            std::sync::Arc::downgrade(&(app.clone() as std::sync::Arc<dyn crate::EndpointApp>));
        app.note_share_active(weak, "mic:builtin", &sid1, Delivery::Pull);
        // 第二个订阅者：复用同一流（不再报「正被使用」）
        let g2 = neg
            .grant(
                "dev-b".into(),
                "设备B".into(),
                Some("mic:builtin".into()),
                None,
                None,
            )
            .unwrap();
        assert_eq!(g2.view.stream_id, sid1, "Push 声明端点仍复用同一流");
        assert_eq!(g2.delivery, Some(Delivery::Pull));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn confirm_subscription_fires_pull_share() {
        use std::sync::Mutex as StdMutex;
        let dir = tmp_dir("share");
        let app = Arc::new(desktop_kernel());
        // 订阅达成应触发端点 share（端点自驱动契约）：注入记录端点记录 ctx
        let fired: Arc<StdMutex<Vec<crate::kernel::SubscribeCtx>>> =
            Arc::new(StdMutex::new(Vec::new()));
        struct RecordingEndpoint {
            base: EndpointBase,
            fired: Arc<StdMutex<Vec<crate::kernel::SubscribeCtx>>>,
        }
        impl Endpoint for RecordingEndpoint {
            fn id(&self) -> &str {
                &self.base.id
            }
            fn kind(&self) -> MediaKind {
                self.base.kind
            }
            fn name(&self) -> &str {
                &self.base.name
            }
            fn target(&self) -> crate::kernel::TargetKind {
                crate::kernel::TargetKind::Determined
            }
            fn transport_profile(&self) -> stross_proto::message::ReliabilityProfile {
                stross_proto::message::ReliabilityProfile::Lossless
            }
            fn strategy(&self) -> stross_proto::message::EndpointStrategy {
                stross_proto::message::EndpointStrategy {
                    strategy_id: stross_proto::message::EndpointStrategy::DEFAULT_ID.into(),
                    serialize: stross_proto::message::SerializeRule::Passthrough,
                    pick: stross_proto::message::PickRule::StrictOrdered,
                }
            }
        }
        impl ShareEndpoint for RecordingEndpoint {
            fn available(&self) -> bool {
                self.base.available
            }
            fn last_error(&self) -> Option<&str> {
                self.base.last_error.as_deref()
            }
            fn load(&mut self) -> Result<(), String> {
                self.base.available = true;
                Ok(())
            }
            fn share(
                &self,
                _app: std::sync::Arc<dyn stross_endpoint::contract::EndpointApp>,
                ctx: stross_endpoint::SubscribeCtx,
            ) {
                self.fired.lock().unwrap().push(ctx);
            }
        }
        app.seed_endpoint(Box::new(RecordingEndpoint {
            base: EndpointBase {
                id: "rec:0".into(),
                kind: MediaKind::File,
                name: "记录".into(),
                available: false,
                last_error: None,
            },
            fired: fired.clone(),
        }));
        let m = app
            .publish_endpoint("rec:0", Visibility::Public, Delivery::Pull, None, None)
            .unwrap();

        let neg = ShareNegotiator {
            app: app.clone(),
            store: Arc::new(TrustStore::load(&dir)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            task: tokio::spawn(async {}),
            port: 0,
        };
        // 挂起条目（订阅方仅声明拉取意向，无自签凭证——订阅驱动只走 pull）；
        // 人工应答允许 → 触发端点 share
        let (tx, rx) = oneshot::channel();
        neg.pending.lock_poisoned().insert(
            "np1".into(),
            PendingEntry {
                device_id: "dev-sub".into(),
                device_name: "订阅方".into(),
                endpoint_id: Some(m.endpoint_id.clone()),
                strategy_id: None,
                delivery_mode: Some(Delivery::Pull),
                relay_addr: None,
                share_token: None,
                tx,
            },
        );
        neg.respond("np1", true, false).unwrap();
        let grant = rx.await.unwrap().expect("应签发 pull 授予");
        assert_eq!(grant.delivery, Some(Delivery::Pull));
        let ctxs = fired.lock().unwrap();
        assert_eq!(ctxs.len(), 1, "确认后应触发一次端点 share");
        let ctx = &ctxs[0];
        assert_eq!(ctx.subscriber, "dev-sub");
        assert_eq!(ctx.delivery, Delivery::Pull);
        assert_eq!(
            ctx.stream_id, grant.view.stream_id,
            "pull 流 id 取自公开方签发的会话"
        );
        assert!(ctx.relay_addr.is_none());
        assert!(ctx.share_token.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn policy_decision_respects_visibility() {
        let dir = tmp_dir("pol");
        let store = TrustStore::load(&dir);
        let app = desktop_kernel();

        // Public：任何人免确认
        app.publish_endpoint(
            "mic:builtin",
            Visibility::Public,
            Delivery::Pull,
            None,
            None,
        )
        .unwrap();
        let m = app.endpoint_manifest("mic:builtin").unwrap();
        assert_eq!(
            policy_decision(&store, Some(&m), "stranger"),
            Decision::Grant
        );

        // Confirm：未信任挂起，信任后自动
        app.publish_endpoint("screen:0", Visibility::Confirm, Delivery::Pull, None, None)
            .unwrap();
        let m = app.endpoint_manifest("screen:0").unwrap();
        assert_eq!(
            policy_decision(&store, Some(&m), "dev-trusted"),
            Decision::Pending
        );
        store.remember("dev-trusted", "可信设备");
        assert_eq!(
            policy_decision(&store, Some(&m), "dev-trusted"),
            Decision::Grant
        );

        // Private：白名单自动、非白名单拒绝
        app.publish_endpoint(
            "sysaudio:builtin",
            Visibility::Private {
                nodes: vec!["dev-ok".into()],
            },
            Delivery::Pull,
            None,
            None,
        )
        .unwrap();
        let m = app.endpoint_manifest("sysaudio:builtin").unwrap();
        assert_eq!(policy_decision(&store, Some(&m), "dev-ok"), Decision::Grant);
        assert_eq!(
            policy_decision(&store, Some(&m), "dev-no"),
            Decision::Reject("请求方不在该端点的白名单内")
        );

        // 旧语义（无端点）：信任自动、未信任挂起
        assert_eq!(
            policy_decision(&store, None, "dev-trusted"),
            Decision::Grant
        );
        assert_eq!(policy_decision(&store, None, "stranger"), Decision::Pending);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn endpoint_unknown_and_private_directory_rules() {
        let app = desktop_kernel();
        // 未知端点查不到
        assert!(app.endpoint_manifest("nope").is_none());
        // 目录快照含全部端点（单层模型）；未通告时 published 全 false
        let endpoints = app.endpoint_catalog();
        assert_eq!(endpoints.len(), 3, "桌面平台默认 3 个端点");
        assert!(endpoints.iter().all(|e| !e.published), "未通告时均不可订阅");
        assert!(endpoints.iter().all(|e| e.available), "探测恒成功 fixture");
    }

    #[test]
    fn unavailable_endpoint_rejected_by_publish_and_subscribe() {
        // 屏幕端点探测失败（无图形会话）→ 不可挂载：拒绝通告；订阅握手 404
        let app = Kernel::new(Platform::Desktop);
        app.seed_endpoint(Box::new(ScreenEndpoint::new(
            "屏幕",
            std::sync::Arc::new(|| Err("无图形会话".into())),
        )));
        let err = app
            .publish_endpoint("screen:0", Visibility::Public, Delivery::Pull, None, None)
            .unwrap_err();
        assert!(
            err.to_string().contains("无图形会话"),
            "通告失败应携带 load 探测原因: {err}"
        );
        // 订阅握手对不可挂载端点返回 404 + 原因
        let rt = tokio::runtime::Runtime::new().unwrap();
        let state = Arc::new(ServerState {
            app: Arc::new(app),
            store: Arc::new(TrustStore::load(&tmp_dir("unavail"))),
            ui: Arc::new(crate::NoopUi),
            pending: Arc::new(Mutex::new(HashMap::new())),
        });
        let req = ShareRequest {
            device_id: "dev-x".into(),
            device_name: "申请方".into(),
            endpoint_id: Some("screen:0".into()),
            strategy_id: None,
            delivery_mode: None,
            relay_addr: None,
            share_token: None,
            media: vec![MediaKind::Screen],
        };
        let (code, body) = rt.block_on(handle_request(State(state), Json(req)));
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert!(
            body.0
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .contains("无图形会话"),
            "握手拒绝应携带原因: {body:?}"
        );
    }
}
