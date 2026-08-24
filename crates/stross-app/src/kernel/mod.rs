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
mod graph;
mod session;

pub use auth::{AuthError, AuthPolicy, PinAuthPolicy};
pub use graph::{Endpoint, NodeInfo, NodeRole};
pub use session::{Negotiated, Session, SessionPrefs};

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::Serialize;
use tokio::sync::broadcast;

use stross_proto::message::{CapabilityDescriptor, RoutePath};

use self::graph::DeviceGraph;
use self::session::{Router, SessionManager};

/// 内核事件（推给 UI，替代轮询）。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum KernelEvent {
    SessionStarted { session: Session },
    SessionRouted { session_id: String, path: RoutePath },
    SessionEnded { session_id: String },
}

/// 内核门面。
pub struct Kernel {
    graph: DeviceGraph,
    sessions: SessionManager,
    auth: Arc<dyn AuthPolicy>,
    next_id: AtomicU64,
    events: broadcast::Sender<KernelEvent>,
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
        }
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
    pub fn create_session(
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
        let mut transports: Vec<String> = caps
            .iter()
            .flat_map(|c| c.transports.iter().cloned())
            .collect();
        transports.sort();
        transports.dedup();
        let transport = match &prefs.preferred_transport {
            Some(t) if transports.is_empty() || transports.contains(t) => t.clone(),
            _ => {
                if transports.iter().any(|t| t == "webrtc") {
                    "webrtc".to_string()
                } else {
                    "ws".to_string()
                }
            }
        };
        let codec = caps
            .iter()
            .flat_map(|c| c.codecs.iter().cloned())
            .next()
            .unwrap_or_else(|| "h264".to_string());
        Negotiated {
            transport,
            codec,
            profile: prefs.profile,
        }
    }

    /// 拆除会话（同样受访问码鉴权约束）。
    pub fn teardown(&self, id: &str) -> Result<(), String> {
        {
            let mut guard = self.sessions.sessions.lock().unwrap();
            let session = guard
                .get_mut(id)
                .ok_or_else(|| format!("会话 {id} 不存在"))?;
            session.require_authorized()?;
            guard.remove(id);
        }
        self.auth.set_code(id, None); // 清理访问码
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

    #[test]
    fn create_session_requires_sinks() {
        let k = Kernel::new();
        assert!(
            k.create_session("a", &[], &SessionPrefs::default())
                .is_err()
        );
    }

    #[test]
    fn session_lifecycle_events() {
        let k = Kernel::new();
        let mut rx = k.subscribe();

        let s = k
            .create_session("a", &["b".into()], &SessionPrefs::default())
            .unwrap();
        assert_eq!(s.path, RoutePath::Direct { node: "b".into() });
        match rx.recv().now_or_never().unwrap().unwrap() {
            KernelEvent::SessionStarted { session } => assert_eq!(session.id, s.id),
            other => panic!("期望 SessionStarted，得到 {other:?}"),
        }

        // 多接收端 → 组播（会再发一个 SessionStarted，先消费掉）
        let m = k
            .create_session("a", &["b".into(), "c".into()], &SessionPrefs::default())
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
        k.teardown(&s.id).unwrap();
        assert!(k.session(&s.id).is_none());
        match rx.recv().now_or_never().unwrap().unwrap() {
            KernelEvent::SessionEnded { session_id } => assert_eq!(session_id, s.id),
            other => panic!("期望 SessionEnded，得到 {other:?}"),
        }
    }

    #[test]
    fn route_unknown_session_fails() {
        let k = Kernel::new();
        assert!(
            k.route("nope", RoutePath::Direct { node: "b".into() })
                .is_err()
        );
        assert!(k.teardown("nope").is_err());
    }

    #[test]
    fn negotiate_picks_transport_and_codec() {
        use stross_proto::message::{CapabilityKind, MediaKind};
        let k = Kernel::new();
        k.upsert_node(NodeInfo {
            node_id: "a".into(),
            name: "a".into(),
            roles: vec![NodeRole::Sender],
            caps: vec![CapabilityDescriptor {
                kind: CapabilityKind::Source,
                media: vec![MediaKind::Screen],
                codecs: vec!["h264".into(), "aac".into()],
                transports: vec!["ws".into()],
                max_width: Some(1920),
                max_height: Some(1080),
                preferred_profile: ReliabilityProfile::Lossy,
            }],
            endpoints: vec![],
        });
        // 源只支持 ws → 协商出 ws + h264
        let s = k
            .create_session("a", &["b".into()], &SessionPrefs::default())
            .unwrap();
        assert_eq!(s.negotiated.transport, "ws");
        assert_eq!(s.negotiated.codec, "h264");
        // 显式偏好 webrtc 但源不支持 → 回退 ws
        let prefs = SessionPrefs {
            profile: ReliabilityProfile::Lossy,
            preferred_transport: Some("webrtc".into()),
            access_code: None,
        };
        let s2 = k.create_session("a", &["b".into()], &prefs).unwrap();
        assert_eq!(s2.negotiated.transport, "ws");
    }

    #[test]
    fn pin_gates_control_operations() {
        use stross_proto::message::RoutePath;
        let k = Kernel::new();
        // 设置访问码创建会话
        let prefs = SessionPrefs {
            profile: ReliabilityProfile::Lossy,
            preferred_transport: None,
            access_code: Some("1234".into()),
        };
        let s = k.create_session("a", &["b".into()], &prefs).unwrap();
        assert!(s.requires_pin);
        // 未授权：route / teardown 都被拒绝
        assert!(
            k.route(&s.id, RoutePath::ViaRelay { node: "r".into() })
                .is_err(),
            "未授权 route 应被拒绝"
        );
        assert!(k.teardown(&s.id).is_err(), "未授权 teardown 应被拒绝");
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
        assert!(k.teardown(&s.id).is_ok());
        // 会话不存在
        assert!(k.authorize("nope", Some("1234")).is_err());
    }

    #[test]
    fn no_pin_session_stays_open() {
        let k = Kernel::new();
        let s = k
            .create_session("a", &["b".into()], &SessionPrefs::default())
            .unwrap();
        assert!(!s.requires_pin);
        assert!(
            k.route(&s.id, RoutePath::ViaRelay { node: "r".into() })
                .is_ok(),
            "无访问码会话应直接放行"
        );
    }
}
