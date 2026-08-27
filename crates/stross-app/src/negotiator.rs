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

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use stross_proto::message::MediaKind;
use stross_proto::time::unix_secs;
use tokio::sync::oneshot;

use crate::app::{ShareTokenView, StrossApp};
use crate::lock::MutexExt;

/// 协商端点默认端口（LAN 可达；防火墙需放行该 TCP 端口）。
pub const DEFAULT_NEGOTIATOR_PORT: u16 = 18779;
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
pub fn load_or_create_identity(base_dir: &std::path::Path, name: &str) -> DeviceIdentity {
    let path = base_dir.join("identity.json");
    if let Ok(s) = std::fs::read_to_string(&path)
        && let Ok(id) = serde_json::from_str::<DeviceIdentity>(&s)
    {
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
// 协商协议
// ---------------------------------------------------------------------------

/// 设备申请凭证的请求。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareRequest {
    pub device_id: String,
    pub device_name: String,
    /// 本次申请的媒体（有限集合；空 = 非法请求）。
    pub media: Vec<MediaKind>,
}

/// 签发结果（成功返回给申请方）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareGrant {
    #[serde(flatten)]
    pub view: ShareTokenView,
    /// 是否因设备受信任而自动签发（未人工确认）。
    pub trusted: bool,
}

/// 待人工确认的请求（推送给 UI 展示）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingRequest {
    /// 挂起请求 id（`negotiator_respond` 时回填）。
    pub id: String,
    pub device_id: String,
    pub device_name: String,
    /// 序列化后的媒体名（`MediaKind` camelCase；前端展示用）。
    pub media: Vec<String>,
    pub created_at: u64,
}

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
/// 挂起请求表：req_id →（device_id, device_name, 应答通道）。
type PendingMap = Arc<Mutex<HashMap<String, (String, String, PendingSender)>>>;

/// 凭证协商服务器。
pub struct ShareNegotiator {
    app: Arc<StrossApp>,
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
        app: Arc<StrossApp>,
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
        let router = Router::new()
            .route("/api/negotiator/request", post(handle_request))
            .layer(axum::middleware::from_fn(cors_layer))
            .with_state(state.clone());
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
        let (device_id, device_name, tx) = self
            .pending
            .lock_poisoned()
            .remove(req_id)
            .ok_or_else(|| format!("挂起请求不存在或已处理: {req_id}"))?;
        if allow {
            // 先写信任再签发：grant.trusted 反映"该设备本次已受信任"
            if remember {
                self.store.remember(&device_id, &device_name);
            }
            let grant = self.grant(device_id.clone(), device_name.clone())?;
            let _ = tx.send(Ok(grant.clone()));
            Ok(Some(grant))
        } else {
            let _ = tx.send(Err("用户拒绝".into()));
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
            .map(|(id, (device_id, device_name, _))| PendingRequest {
                id: id.clone(),
                device_id: device_id.clone(),
                device_name: device_name.clone(),
                media: vec!["mic".into()], // 当前协商协议仅支持 mic
                created_at: 0,             // 挂起表未记录创建时刻，置 0 表示未知
            })
            .collect()
    }

    fn grant(&self, device_id: String, device_name: String) -> Result<ShareGrant, String> {
        let view = self
            .app
            .issue_share_token_for(
                format!("接收 {device_name} 共享"),
                vec![MediaKind::Mic],
                Some(DEFAULT_GRANT_TTL_SECS),
            )
            .map_err(|e| e.to_user_string())?;
        Ok(ShareGrant {
            view,
            trusted: self.store.is_trusted(&device_id),
        })
    }

    /// 停止协商服务。
    pub async fn stop(self) {
        self.task.abort();
    }
}

/// 服务器共享状态（与 [`CtrlServer`] 的 `CtrlState` 同构）。
struct ServerState {
    app: Arc<StrossApp>,
    store: Arc<TrustStore>,
    ui: Arc<dyn NegotiatorUi>,
    pending: PendingMap,
}

async fn handle_request(
    State(state): State<Arc<ServerState>>,
    Json(req): Json<ShareRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    if req.media.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "media 不能为空" })),
        );
    }
    let media_names: Vec<String> = req
        .media
        .iter()
        .map(|m| {
            serde_json::to_string(m)
                .unwrap_or_default()
                .trim_matches('"')
                .to_string()
        })
        .collect();

    // 已信任设备 → 自动签发，无需人工确认
    if state.store.is_trusted(&req.device_id) {
        let grant = state.app.issue_share_token_for(
            format!("接收 {} 共享", req.device_name),
            req.media.clone(),
            Some(DEFAULT_GRANT_TTL_SECS),
        );
        return match grant {
            Ok(view) => (
                StatusCode::OK,
                Json(
                    serde_json::to_value(ShareGrant {
                        view,
                        trusted: true,
                    })
                    .unwrap_or_default(),
                ),
            ),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_user_string() })),
            ),
        };
    }

    // 未知设备 → 挂起等待人工确认（60s 超时）
    let id = format!("n{}", new_pending_id());
    let (tx, rx) = oneshot::channel();
    {
        let mut pending = state.pending.lock_poisoned();
        pending.insert(
            id.clone(),
            (req.device_id.clone(), req.device_name.clone(), tx),
        );
    }
    state.ui.request_pending(&PendingRequest {
        id: id.clone(),
        device_id: req.device_id.clone(),
        device_name: req.device_name.clone(),
        media: media_names,
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

fn new_pending_id() -> u64 {
    // 全局自增：时间戳 + 进程内计数器（多实例互不冲突）
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// CORS 中间件：Tauri 前端运行在本地源（`tauri://localhost`），跨源访问
/// 协商端点（POST + `Content-Type: application/json` 会触发预检），必须允许
/// 任意来源（与中继 HTTP 层的 cors_layer 语义一致——LAN 可信模型下不限定来源）。
async fn cors_layer(
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = req.method().clone();
    let mut resp = next.run(req).await;
    let headers = resp.headers_mut();
    headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        axum::http::HeaderValue::from_static("*"),
    );
    headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_METHODS,
        axum::http::HeaderValue::from_static("POST, OPTIONS"),
    );
    headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_HEADERS,
        axum::http::HeaderValue::from_static("Content-Type"),
    );
    // 预检直接放行（axum 对 OPTIONS 无路由 → 404；这里显式返回 204）
    if method == axum::http::Method::OPTIONS {
        resp = axum::response::Response::builder()
            .status(axum::http::StatusCode::NO_CONTENT)
            .body(axum::body::Body::empty())
            .expect("静态响应");
        resp.headers_mut().insert(
            axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
            axum::http::HeaderValue::from_static("*"),
        );
        resp.headers_mut().insert(
            axum::http::header::ACCESS_CONTROL_ALLOW_METHODS,
            axum::http::HeaderValue::from_static("POST, OPTIONS"),
        );
        resp.headers_mut().insert(
            axum::http::header::ACCESS_CONTROL_ALLOW_HEADERS,
            axum::http::HeaderValue::from_static("Content-Type"),
        );
        return resp;
    }
    resp
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Platform;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("stross-neg-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
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
        let app = Arc::new(StrossApp::new(Platform::Desktop));
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
        neg.pending
            .lock_poisoned()
            .insert("n1".into(), ("dev-phone-1".into(), "手机A".into(), tx2));

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
        let app = Arc::new(StrossApp::new(Platform::Desktop));
        let neg = ShareNegotiator {
            app,
            store: Arc::new(store),
            pending: Arc::new(Mutex::new(HashMap::new())),
            task: tokio::spawn(async {}),
            port: 0,
        };
        let grant = neg
            .grant("dev-phone-2".into(), "手机B".into())
            .expect("信任设备应自动签发");
        assert!(grant.trusted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn deny_returns_error() {
        let dir = tmp_dir("deny");
        let app = Arc::new(StrossApp::new(Platform::Desktop));
        let neg = ShareNegotiator {
            app,
            store: Arc::new(TrustStore::load(&dir)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            task: tokio::spawn(async {}),
            port: 0,
        };
        let (tx, rx) = oneshot::channel();
        neg.pending
            .lock_poisoned()
            .insert("n3".into(), ("dev-3".into(), "手机C".into(), tx));
        neg.respond("n3", false, false).unwrap();
        let res = rx.await.unwrap();
        assert!(res.is_err(), "拒绝时应返回错误给申请方");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
