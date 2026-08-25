//! 内核（控制面）骨架：设备图 / 会话管理 / 路由 / 鉴权。
//!
//! 见 docs/plugin-architecture.md §3——内核与编解码、传输完全解耦，
//! 只负责：
//!
//! * **设备图**（[`graph`]）：局域网内节点的能力注册与发现结果聚合
//! * **会话管理**（[`session`]）：会话拓扑（source → sinks[]）与协商结果
//! * **路由**（[`session::Router`]）：传输方向控制（直连 / 经中继 / 组播）
//! * **鉴权**（[`auth`]）：会话级访问码（PIN）策略
//!
//! 阶段 0 仅提供骨架与路由 API（`create_session` / `route` / `teardown`），
//! 不接真实传输协商（阶段 1 落地）；所有变更通过 [`KernelEvent`] 广播给 UI。

mod auth;
mod data_plane;
mod graph;
mod session;

pub use auth::{AuthError, AuthPolicy, PinAuthPolicy};
pub use data_plane::{DataPlaneBackend, RelayDataPlane};
pub use graph::{Endpoint, NodeInfo, NodeRole};
pub use session::{Negotiated, Session, SessionPrefs};

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use stross_core::relay::RelayEvent;
use stross_proto::message::{
    CapabilityDescriptor, CodecId, MediaKind, RoutePath, ShareToken, StreamInfo, TransportId,
};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use self::graph::DeviceGraph;
use self::session::{Router, SessionManager};

/// 内核事件（推给 UI，替代轮询）。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum KernelEvent {
    SessionStarted {
        session: Session,
    },
    SessionRouted {
        session_id: String,
        path: RoutePath,
    },
    SessionEnded {
        session_id: String,
    },
    /// 数据面流启动（内嵌中继上报；D4：session_id 与 stream_id 合一）。
    StreamStarted {
        session_id: String,
        info: StreamInfo,
    },
    /// 数据面流结束。
    StreamEnded {
        session_id: String,
    },
    /// 观看者数量变化。
    WatchersChanged {
        session_id: String,
        watchers: u32,
    },
}

/// 内核门面。
pub struct Kernel {
    graph: DeviceGraph,
    sessions: SessionManager,
    auth: Arc<dyn AuthPolicy>,
    next_id: AtomicU64,
    events: broadcast::Sender<KernelEvent>,
    /// 数据面后端（内嵌受控中继等；`None` = 未接线，会话不驱动数据面）。
    data_plane: std::sync::Mutex<Option<Arc<dyn DataPlaneBackend>>>,
    /// 数据面事件转发任务（[`RelayEvent`] → [`KernelEvent`]）。
    data_plane_task: std::sync::Mutex<Option<JoinHandle<()>>>,
    /// 接入凭证签发表（stream_id → 签发时的完整凭证；`Arc` 供数据面校验器共享，
    /// 校验器只持本表引用，不形成循环引用）。
    share_tokens: Arc<std::sync::Mutex<HashMap<String, ShareToken>>>,
}

impl Default for Kernel {
    fn default() -> Self {
        Self::new()
    }
}

impl Kernel {
    pub fn new() -> Self {
        Self::with_auth(Arc::new(PinAuthPolicy::default()))
    }

    /// 注入自定义鉴权策略（远期 WASM 插件等）。
    pub fn with_auth(auth: Arc<dyn AuthPolicy>) -> Self {
        let (events, _rx) = broadcast::channel(64);
        Self {
            graph: DeviceGraph::default(),
            sessions: SessionManager::default(),
            auth,
            next_id: AtomicU64::new(1),
            events,
            data_plane: std::sync::Mutex::new(None),
            data_plane_task: std::sync::Mutex::new(None),
            share_tokens: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    // -----------------------------------------------------------------------
    // 数据面接线
    // -----------------------------------------------------------------------

    /// 接入数据面后端（内嵌受控中继）：订阅其流生命周期事件并转发为
    /// [`KernelEvent`]（StreamStarted / StreamEnded / WatchersChanged）；
    /// 同时注入接入凭证校验器（B 阶段跨设备推流：受控中继在预授权之外
    /// 接受本内核签发的 [`ShareToken`]）。
    pub fn attach_data_plane(&self, backend: Arc<dyn DataPlaneBackend>) {
        *self.data_plane.lock().unwrap() = Some(backend.clone());
        backend.set_share_token_validator(self.token_validator());
        let mut rx = backend.events();
        let events = self.events.clone();
        let task = tokio::spawn(async move {
            while let Ok(ev) = rx.recv().await {
                let kernel_ev = match ev {
                    RelayEvent::StreamStarted { stream_id, info } => KernelEvent::StreamStarted {
                        session_id: stream_id,
                        info,
                    },
                    RelayEvent::StreamEnded { stream_id } => KernelEvent::StreamEnded {
                        session_id: stream_id,
                    },
                    RelayEvent::WatchersChanged {
                        stream_id,
                        watchers,
                    } => KernelEvent::WatchersChanged {
                        session_id: stream_id,
                        watchers,
                    },
                };
                let _ = events.send(kernel_ev);
            }
        });
        *self.data_plane_task.lock().unwrap() = Some(task);
    }

    // -----------------------------------------------------------------------
    // 接入凭证（B 阶段：凭证式跨设备推流，见 docs/iteration-plan.md B0/B1）
    // -----------------------------------------------------------------------

    /// 为已建会话签发一次性接入凭证（`ttl` 为有效期；`media` 为本次共享类型）。
    ///
    /// 调用方（控制面 / GUI）把凭证编码为二维码 / 短码交给推流端（如手机）；
    /// 推流端在 Hello 中出示凭证即可接入本机受控中继，**无需任何远程控制面**。
    pub fn create_share_token(
        &self,
        session_id: &str,
        media: Vec<MediaKind>,
        ttl: Duration,
    ) -> Result<ShareToken, String> {
        if !self.has_session(session_id) {
            return Err(format!("会话 {session_id} 不存在"));
        }
        let now = now_secs();
        // 惰性清理过期凭证，保持签发表有界
        let mut tokens = self.share_tokens.lock().unwrap();
        tokens.retain(|_, t| !t.is_expired(now));
        let token = ShareToken {
            v: ShareToken::VERSION,
            stream_id: session_id.to_string(),
            pin: random_pin(session_id),
            expires_at: now.saturating_add(ttl.as_secs()),
            media,
        };
        tokens.insert(session_id.to_string(), token.clone());
        Ok(token)
    }

    /// 校验凭证：已签发 + 未过期 + 与签发时逐字一致（防篡改 / 重放）。
    pub fn verify_share_token(&self, token: &ShareToken) -> Result<(), String> {
        let tokens = self.share_tokens.lock().unwrap();
        let stored = tokens
            .get(&token.stream_id)
            .ok_or_else(|| format!("凭证无效：会话 {} 未签发凭证", token.stream_id))?;
        if stored != token {
            return Err("凭证无效：与签发时不符（可能被篡改或重放）".into());
        }
        if stored.is_expired(now_secs()) {
            return Err("凭证已过期".into());
        }
        Ok(())
    }

    /// 数据面凭证校验器（读本内核签发表；注入受控中继用）。
    pub fn token_validator(&self) -> Arc<dyn stross_core::relay::ShareTokenValidator> {
        Arc::new(KernelTokenValidator {
            tokens: self.share_tokens.clone(),
        })
    }

    /// 是否已接入数据面。
    pub fn has_data_plane(&self) -> bool {
        self.data_plane.lock().unwrap().is_some()
    }

    /// 会话是否存在（id 已由内核签发且未拆除）。
    pub fn has_session(&self, id: &str) -> bool {
        self.sessions.sessions.lock().unwrap().contains_key(id)
    }

    // -----------------------------------------------------------------------
    // 设备图
    // -----------------------------------------------------------------------

    /// 注册/更新一个节点（发现结果、本机能力都走这里）。
    pub fn upsert_node(&self, node: NodeInfo) {
        self.graph
            .nodes
            .lock()
            .unwrap()
            .insert(node.node_id.clone(), node);
    }

    /// 给已有节点追加一条能力（重复条目按 `media` 去重）。
    pub fn register_capability(&self, node_id: &str, desc: CapabilityDescriptor) {
        let mut guard = self.graph.nodes.lock().unwrap();
        if let Some(node) = guard.get_mut(node_id)
            && !node.caps.contains(&desc)
        {
            node.caps.push(desc);
        }
    }

    /// 当前设备图快照（按节点 id 排序）。
    pub fn nodes(&self) -> Vec<NodeInfo> {
        let guard = self.graph.nodes.lock().unwrap();
        let mut v: Vec<_> = guard.values().cloned().collect();
        v.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        v
    }

    // -----------------------------------------------------------------------
    // 会话
    // -----------------------------------------------------------------------

    /// 创建会话（「从 `src` 推送到 `sinks`」）。
    ///
    /// 阶段 1：根据源节点能力做**最简协商**（传输偏好 ∩ 源能力、编解码取源能力
    /// 第一项），填充 [`Session::negotiated`]；完整的线上 Offer/Answer 在
    /// 传输信令层完成（如 WebRTC 的 `/api/webrtc/*`）。
    ///
    /// 已接入数据面（[`Kernel::attach_data_plane`]）时，会话 id 由内核签发并
    /// **预授权**给受控中继（需求 F2.2「先会话后传输」/ D4：id 与 stream_id 合一）。
    pub async fn create_session(
        &self,
        src: &str,
        sinks: &[String],
        prefs: &SessionPrefs,
    ) -> Result<Session, String> {
        if sinks.is_empty() {
            return Err("会话至少需要一个接收端（sinks）".into());
        }
        let id = format!("sess-{:x}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let requires_pin = prefs.access_code.is_some();
        if requires_pin {
            self.auth.set_code(&id, prefs.access_code.as_deref());
        }
        // 数据面预授权：先授权成功再登记会话，避免"会话已建但无法推流"的中间态
        // （先 clone 出 Arc 再 await，避免 MutexGuard 跨 await 使 future 非 Send）
        let dp = self.data_plane.lock().unwrap().clone();
        if let Some(dp) = dp {
            dp.authorize_stream(&id)
                .await
                .map_err(|e| format!("数据面预授权失败: {e}"))?;
        }
        let session = Session {
            id,
            source: src.to_string(),
            sinks: sinks.to_vec(),
            path: Router::default_path(sinks),
            negotiated: self.negotiate(src, prefs),
            requires_pin,
            authorized: !requires_pin, // 无访问码的会话控制面直接放行（现状行为）
        };
        self.sessions
            .sessions
            .lock()
            .unwrap()
            .insert(session.id.clone(), session.clone());
        let _ = self.events.send(KernelEvent::SessionStarted {
            session: session.clone(),
        });
        Ok(session)
    }

    /// 控制传输方向：会话存续期间动态改道。
    ///
    /// 会话启用访问码（PIN）且未通过 [`Kernel::authorize`] 时拒绝（设计文档 §7）。
    pub fn route(&self, id: &str, path: RoutePath) -> Result<(), String> {
        let mut guard = self.sessions.sessions.lock().unwrap();
        let session = guard
            .get_mut(id)
            .ok_or_else(|| format!("会话 {id} 不存在"))?;
        session.require_authorized()?;
        session.path = path.clone();
        drop(guard);
        let _ = self.events.send(KernelEvent::SessionRouted {
            session_id: id.to_string(),
            path,
        });
        Ok(())
    }

    /// 会话鉴权：校验访问码；成功后该会话的控制操作放行。
    ///
    /// 未设置访问码的会话直接成功（无操作）。
    pub fn authorize(&self, id: &str, access_code: Option<&str>) -> Result<(), String> {
        let mut guard = self.sessions.sessions.lock().unwrap();
        let session = guard
            .get_mut(id)
            .ok_or_else(|| format!("会话 {id} 不存在"))?;
        self.auth
            .authorize(id, access_code)
            .map_err(|e| e.to_string())?;
        session.mark_authorized();
        Ok(())
    }

    /// 查询单个会话。
    pub fn session(&self, id: &str) -> Option<Session> {
        self.sessions.sessions.lock().unwrap().get(id).cloned()
    }

    /// 会话列表快照（按 id 排序）。
    pub fn sessions(&self) -> Vec<Session> {
        let guard = self.sessions.sessions.lock().unwrap();
        let mut v: Vec<_> = guard.values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    /// 最简能力协商（阶段 1）：
    /// * 传输：`prefs.preferred_transport` ∩ 源能力；未指定时源支持 webrtc 则用
    ///   webrtc，否则 ws（推流现状）
    /// * 编解码：源能力第一项（默认 h264）
    fn negotiate(&self, src: &str, prefs: &SessionPrefs) -> Negotiated {
        let caps: Vec<CapabilityDescriptor> = self
            .graph
            .nodes
            .lock()
            .unwrap()
            .get(src)
            .map(|n| n.caps.clone())
            .unwrap_or_default();
        let mut transports: Vec<TransportId> = caps
            .iter()
            .flat_map(|c| c.transports.iter().copied())
            .collect();
        transports.sort();
        transports.dedup();
        let transport = match &prefs.preferred_transport {
            Some(t) if transports.is_empty() || transports.contains(t) => *t,
            _ => {
                if transports.contains(&TransportId::WebRtc) {
                    TransportId::WebRtc
                } else {
                    TransportId::Ws
                }
            }
        };
        let codec = caps
            .iter()
            .flat_map(|c| c.codecs.iter().copied())
            .next()
            .unwrap_or(CodecId::H264);
        Negotiated {
            transport,
            codec,
            profile: prefs.profile,
        }
    }

    /// 拆除会话（同样受访问码鉴权约束）；已接入数据面时撤销流预授权。
    pub async fn teardown(&self, id: &str) -> Result<(), String> {
        {
            let mut guard = self.sessions.sessions.lock().unwrap();
            let session = guard
                .get_mut(id)
                .ok_or_else(|| format!("会话 {id} 不存在"))?;
            session.require_authorized()?;
            guard.remove(id);
        }
        self.auth.set_code(id, None); // 清理访问码
        let dp = self.data_plane.lock().unwrap().clone();
        if let Some(dp) = dp {
            dp.revoke_stream(id)
                .await
                .map_err(|e| format!("数据面撤销失败: {e}"))?;
        }
        let _ = self.events.send(KernelEvent::SessionEnded {
            session_id: id.to_string(),
        });
        Ok(())
    }

    /// 订阅内核事件。
    pub fn subscribe(&self) -> broadcast::Receiver<KernelEvent> {
        self.events.subscribe()
    }
}

/// 数据面接入凭证校验器：读内核签发表，校验"存在 + 未过期 + 逐字一致"。
struct KernelTokenValidator {
    tokens: Arc<std::sync::Mutex<HashMap<String, ShareToken>>>,
}

impl stross_core::relay::ShareTokenValidator for KernelTokenValidator {
    fn validate(&self, token: &ShareToken) -> bool {
        let tokens = self.tokens.lock().unwrap();
        let Some(stored) = tokens.get(&token.stream_id) else {
            return false;
        };
        stored == token && !stored.is_expired(now_secs())
    }
}

/// 当前 Unix 秒。
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 一次性凭证 PIN（6 位数字）。
///
/// 非密码学随机（一次性凭证防误连/旁观冒用即可）：`DefaultHasher` 每次运行
/// 带进程随机种子，混合会话 id 与纳秒时间，碰撞概率可忽略；不引入 rand 依赖。
fn random_pin(seed: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    seed.hash(&mut h);
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut h);
    let v = h.finish();
    format!("{:06}", v % 1_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;
    use stross_proto::message::ReliabilityProfile;

    fn node(id: &str) -> NodeInfo {
        NodeInfo {
            node_id: id.into(),
            name: id.into(),
            roles: vec![NodeRole::Sender],
            caps: vec![],
            endpoints: vec![],
        }
    }

    #[test]
    fn graph_upsert_and_capability() {
        let k = Kernel::new();
        k.upsert_node(node("a"));
        k.upsert_node(node("b"));
        assert_eq!(k.nodes().len(), 2);
        k.register_capability("a", CapabilityDescriptor::unknown());
        k.register_capability("a", CapabilityDescriptor::unknown()); // 去重
        let a = k.nodes().into_iter().find(|n| n.node_id == "a").unwrap();
        assert_eq!(a.caps.len(), 1);
    }

    #[tokio::test]
    async fn create_session_requires_sinks() {
        let k = Kernel::new();
        assert!(
            k.create_session("a", &[], &SessionPrefs::default())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn session_lifecycle_events() {
        let k = Kernel::new();
        let mut rx = k.subscribe();

        let s = k
            .create_session("a", &["b".into()], &SessionPrefs::default())
            .await
            .unwrap();
        assert_eq!(s.path, RoutePath::Direct { node: "b".into() });
        match rx.recv().now_or_never().unwrap().unwrap() {
            KernelEvent::SessionStarted { session } => assert_eq!(session.id, s.id),
            other => panic!("期望 SessionStarted，得到 {other:?}"),
        }

        // 多接收端 → 组播（会再发一个 SessionStarted，先消费掉）
        let m = k
            .create_session("a", &["b".into(), "c".into()], &SessionPrefs::default())
            .await
            .unwrap();
        assert!(matches!(m.path, RoutePath::Mesh { .. }));
        match rx.recv().now_or_never().unwrap().unwrap() {
            KernelEvent::SessionStarted { session } => assert_eq!(session.id, m.id),
            other => panic!("期望 SessionStarted，得到 {other:?}"),
        }

        // 改道
        k.route(
            &s.id,
            RoutePath::ViaRelay {
                node: "relay-1".into(),
            },
        )
        .unwrap();
        assert_eq!(
            k.session(&s.id).unwrap().path,
            RoutePath::ViaRelay {
                node: "relay-1".into()
            }
        );
        match rx.recv().now_or_never().unwrap().unwrap() {
            KernelEvent::SessionRouted { session_id, .. } => assert_eq!(session_id, s.id),
            other => panic!("期望 SessionRouted，得到 {other:?}"),
        }

        // 拆除
        k.teardown(&s.id).await.unwrap();
        assert!(k.session(&s.id).is_none());
        match rx.recv().now_or_never().unwrap().unwrap() {
            KernelEvent::SessionEnded { session_id } => assert_eq!(session_id, s.id),
            other => panic!("期望 SessionEnded，得到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn route_unknown_session_fails() {
        let k = Kernel::new();
        assert!(
            k.route("nope", RoutePath::Direct { node: "b".into() })
                .is_err()
        );
        assert!(k.teardown("nope").await.is_err());
    }

    #[tokio::test]
    async fn negotiate_picks_transport_and_codec() {
        use stross_proto::message::{CapabilityKind, MediaKind};
        let k = Kernel::new();
        k.upsert_node(NodeInfo {
            node_id: "a".into(),
            name: "a".into(),
            roles: vec![NodeRole::Sender],
            caps: vec![CapabilityDescriptor {
                kind: CapabilityKind::Source,
                media: vec![MediaKind::Screen],
                codecs: vec![CodecId::H264, CodecId::Aac],
                transports: vec![TransportId::Ws],
                max_width: Some(1920),
                max_height: Some(1080),
                preferred_profile: ReliabilityProfile::Lossy,
            }],
            endpoints: vec![],
        });
        // 源只支持 ws → 协商出 ws + h264
        let s = k
            .create_session("a", &["b".into()], &SessionPrefs::default())
            .await
            .unwrap();
        assert_eq!(s.negotiated.transport, TransportId::Ws);
        assert_eq!(s.negotiated.codec, CodecId::H264);
        // 显式偏好 webrtc 但源不支持 → 回退 ws
        let prefs = SessionPrefs {
            profile: ReliabilityProfile::Lossy,
            preferred_transport: Some(TransportId::WebRtc),
            access_code: None,
        };
        let s2 = k.create_session("a", &["b".into()], &prefs).await.unwrap();
        assert_eq!(s2.negotiated.transport, TransportId::Ws);
    }

    #[tokio::test]
    async fn pin_gates_control_operations() {
        use stross_proto::message::RoutePath;
        let k = Kernel::new();
        // 设置访问码创建会话
        let prefs = SessionPrefs {
            profile: ReliabilityProfile::Lossy,
            preferred_transport: None,
            access_code: Some("1234".into()),
        };
        let s = k.create_session("a", &["b".into()], &prefs).await.unwrap();
        assert!(s.requires_pin);
        // 未授权：route / teardown 都被拒绝
        assert!(
            k.route(&s.id, RoutePath::ViaRelay { node: "r".into() })
                .is_err(),
            "未授权 route 应被拒绝"
        );
        assert!(k.teardown(&s.id).await.is_err(), "未授权 teardown 应被拒绝");
        // 错误访问码
        assert!(k.authorize(&s.id, Some("9999")).is_err());
        assert!(
            k.route(&s.id, RoutePath::ViaRelay { node: "r".into() })
                .is_err()
        );
        // 正确访问码 → 放行
        assert!(k.authorize(&s.id, Some("1234")).is_ok());
        assert!(
            k.route(&s.id, RoutePath::ViaRelay { node: "r".into() })
                .is_ok()
        );
        assert!(k.teardown(&s.id).await.is_ok());
        // 会话不存在
        assert!(k.authorize("nope", Some("1234")).is_err());
    }

    #[tokio::test]
    async fn no_pin_session_stays_open() {
        let k = Kernel::new();
        let s = k
            .create_session("a", &["b".into()], &SessionPrefs::default())
            .await
            .unwrap();
        assert!(!s.requires_pin);
        assert!(
            k.route(&s.id, RoutePath::ViaRelay { node: "r".into() })
                .is_ok(),
            "无访问码会话应直接放行"
        );
    }

    #[tokio::test]
    async fn share_token_lifecycle() {
        use stross_proto::message::MediaKind;
        let k = Kernel::new();
        let s = k
            .create_session("a", &["b".into()], &SessionPrefs::default())
            .await
            .unwrap();

        // 未知会话 → 拒绝
        assert!(
            k.create_share_token("nope", vec![MediaKind::Mic], Duration::from_secs(60))
                .is_err()
        );

        // 签发：stream_id 与会话一致、PIN 为 6 位数字、有效期正确
        let token = k
            .create_share_token(&s.id, vec![MediaKind::Mic], Duration::from_secs(60))
            .unwrap();
        assert_eq!(token.stream_id, s.id);
        assert_eq!(token.v, ShareToken::VERSION);
        assert!(token.pin.len() == 6 && token.pin.chars().all(|c| c.is_ascii_digit()));
        assert_eq!(token.expires_at, now_secs().saturating_add(60));

        // 校验通过
        assert!(k.verify_share_token(&token).is_ok());

        // 篡改 PIN → 拒绝（逐字比对）
        let mut forged = token.clone();
        forged.pin = "000000".into();
        assert!(k.verify_share_token(&forged).is_err());

        // 篡改 stream_id → 拒绝（查不到签发记录）
        let mut forged2 = token.clone();
        forged2.stream_id = "sess-other".into();
        assert!(k.verify_share_token(&forged2).is_err());

        // 重新签发覆盖旧凭证（同会话最新凭证有效）
        let token2 = k
            .create_share_token(&s.id, vec![MediaKind::Mic], Duration::from_secs(60))
            .unwrap();
        assert!(k.verify_share_token(&token2).is_ok());
        assert!(k.verify_share_token(&token).is_err(), "旧凭证应失效");

        // ttl=0 → 立即过期
        let expired = k
            .create_share_token(&s.id, vec![MediaKind::Mic], Duration::ZERO)
            .unwrap();
        assert!(k.verify_share_token(&expired).is_err());
    }
}
