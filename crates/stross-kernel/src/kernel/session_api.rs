//! 内核控制面域（`impl Kernel`）：设备图 / 会话 / 路由 / 鉴权 / 接入凭证。
//!
//! docs/framework-v3.md：`Kernel` 单一门面；本文件承载「控制面
//! （会话 / 路由 / 鉴权 / 凭证 / 设备图）」一域的实现，方法与公共 API 不变。

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use stross_proto::message::{CapabilityDescriptor, CodecId, MediaKind, ShareToken, TransportId};
use stross_view::ShareTokenView;

use crate::Kernel;
use crate::error::{Error, Result};
use crate::lock::MutexExt;

use super::{
    Id, Negotiated, NodeId, NodeInfo, Session, SessionPrefs, StreamId, now_secs, random_pin,
};

impl Kernel {
    // -----------------------------------------------------------------------
    // 节点图
    // -----------------------------------------------------------------------

    /// 注册/更新一个节点（发现结果、本机能力都走这里）。
    pub fn upsert_node(&self, node: NodeInfo) {
        self.graph.nodes.lock_poisoned().insert(node.node_id, node);
    }

    /// 给已有节点追加一条能力（重复条目按 `media` 去重）。
    pub fn register_capability(&self, node_id: &NodeId, desc: CapabilityDescriptor) {
        let mut guard = self.graph.nodes.lock_poisoned();
        if let Some(node) = guard.get_mut(node_id)
            && !node.caps.contains(&desc)
        {
            node.caps.push(desc);
        }
    }

    /// 当前设备图快照（按节点 id 排序）。
    pub fn nodes(&self) -> Vec<NodeInfo> {
        let guard = self.graph.nodes.lock_poisoned();
        let mut v: Vec<_> = guard.values().cloned().collect();
        v.sort_by_key(|a| a.node_id);
        v
    }

    // -----------------------------------------------------------------------
    // 会话
    // -----------------------------------------------------------------------

    /// 显式 id 会话（幂等；通信模式 v2 语义 id 派生路径，
    /// docs/framework-v3.md §6「配套改动」）：已存在同 id 会话时直接返回
    /// （不重复建、不重复预授权），否则按 id 创建并预授权数据面。
    ///
    /// 语义 id = `derive(endpoint_id, transport_profile, pick_rule)`（确定性）：
    /// 同端点必然同 id → 订阅收敛（同流复用）与停流隔离（互不级联）的结构基础。
    ///
    /// **内部访问器**（v3 P3 方法面收敛）：协商层（`negotiator`）建会话用，
    /// 不再作为 Kernel 门面 `pub fn`（壳层经控制面/协商 API 走，不直调）。
    pub(crate) fn ensure_session_with_id(
        &self,
        id: &str,
        src: &str,
        sinks: &[String],
        prefs: &SessionPrefs,
    ) -> Result<Session> {
        // 边界 `&str` → 内部 `Id`（壳层仍传字符串）
        let id = Id::from(id);
        if let Some(s) = self.sessions.get(&id) {
            return Ok(s);
        }
        self.build_session(id, src, sinks, prefs)
    }

    /// 会话构建公共核心（`create_session` 生成随机 id、`ensure_session_with_id`
    /// 用派生 id，均走本函数）：校验 → 访问码 → 数据面预授权 → 登记 → 事件。
    ///
    /// **内部访问器**（v3 P3 方法面收敛）：控制面命令 `CreateSession` 的处理
    /// 逻辑下沉到 [`crate::control`]（id 签发在控制面侧），本函数是共享构建
    /// 核心，不再作为 Kernel 门面 `pub fn`。
    pub(crate) fn build_session(
        &self,
        id: StreamId,
        src: &str,
        sinks: &[String],
        prefs: &SessionPrefs,
    ) -> Result<Session> {
        if sinks.is_empty() {
            return Err(Error::Message("会话至少需要一个接收端（sinks）".into()));
        }
        let requires_pin = prefs.access_code.is_some();
        if requires_pin {
            self.auth.set_code(&id, prefs.access_code.as_deref());
        }
        // 数据面预授权：先授权成功再登记会话，避免"会话已建但无法推流"的中间态
        // （先 clone 出 Arc 再调用外部后端，不在持内核锁期间执行后端调用）
        let dp = self.data_plane.lock_poisoned().clone();
        if let Some(dp) = dp {
            dp.authorize_stream(&id)
                .map_err(|e| Error::DataPlane(format!("预授权失败: {e}")))?;
        }
        let session = Session {
            id,
            title: prefs.title.clone(),
            source: src.to_string(),
            sinks: sinks.to_vec(),
            path: crate::kernel::session::Router::default_path(sinks),
            negotiated: self.negotiate(src, prefs),
            requires_pin,
            authorized: !requires_pin, // 无访问码的会话控制面直接放行（现状行为）
        };
        self.sessions.insert(session.clone());
        Ok(session)
    }

    /// 会话列表快照（按 id 排序）。
    ///
    /// **内部访问器**（v3 P3 方法面收敛）：控制面 `ListSessions` / `Status`
    /// 命令直连本表，不再作为 Kernel 门面 `pub fn`（壳层经控制面走，不直调）。
    pub(crate) fn sessions(&self) -> Vec<Session> {
        self.sessions.snapshot()
    }

    /// 会话是否存在（id 已由内核签发且未拆除）。
    ///
    /// **内部访问器**（v3 P3 方法面收敛）：凭证签发（`create_share_token`）
    /// 与协商层校验会话存在用，不再作为 Kernel 门面 `pub fn`。
    pub(crate) fn has_session(&self, id: &str) -> bool {
        self.sessions.contains(&Id::from(id))
    }

    /// 最简能力协商：
    /// * 传输：`prefs.preferred_transport` ∩ 源能力；未指定时源支持 webrtc 则用
    ///   webrtc，否则 ws（推流现状）
    /// * 编解码：源能力第一项（默认 h264）
    fn negotiate(&self, src: &str, prefs: &SessionPrefs) -> Negotiated {
        let caps: Vec<CapabilityDescriptor> = self
            .graph
            .nodes
            .lock_poisoned()
            .get(&NodeId::from(src))
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

    /// 内部拆除会话：可选择是否强制校验访问码鉴权（数据面流结束自愈清理 vs 用户指令）。
    fn teardown_internal(&self, id: &str, check_auth: bool) -> Result<()> {
        let id = Id::from(id);
        if check_auth {
            self.sessions.require_authorized(&id)?;
        }
        self.sessions.remove(&id);
        self.auth.set_code(id.as_str(), None); // 清理访问码
        self.share_tokens.lock_poisoned().remove(&id); // 凭证随会话失效（防重放）
        let dp = self.data_plane.lock_poisoned().clone();
        if let Some(dp) = dp {
            dp.revoke_stream(id.as_str())
                .map_err(|e| Error::DataPlane(format!("撤销失败: {e}")))?;
        }
        Ok(())
    }

    /// 拆除会话（受访问码鉴权约束）；已接入数据面时撤销流预授权。
    ///
    /// **内部访问器**（v3 P3 方法面收敛）：控制面 `Teardown` 命令直连，
    /// 不再作为 Kernel 门面 `pub fn`（壳层经控制面走，不直调）。
    pub(crate) fn teardown(&self, id: &str) -> Result<()> {
        self.teardown_internal(id, true)
    }

    /// 强制拆除会话（内部生命周期收尾，不受访问码约束）。
    ///
    /// **内部访问器**（v3 P3 方法面收敛）：数据面流结束自愈清理
    /// （`endpoint_api::reap_stream`）直连，不再作为 Kernel 门面 `pub fn`。
    pub(crate) fn force_teardown(&self, id: &str) -> Result<()> {
        self.teardown_internal(id, false)
    }

    // -----------------------------------------------------------------------
    // 接入凭证（B 阶段：凭证式跨设备推流，见 docs/iteration-plan.md B0/B1）
    // -----------------------------------------------------------------------

    /// 为已建会话签发一次性接入凭证（`ttl` 为有效期；`media` 为本次共享类型）。
    ///
    /// 调用方（控制面 / 协商层）把凭证编码为二维码 / 短码交给推流端（如手机）；
    /// 推流端在 Hello 中出示凭证即可接入本机受控中继，**无需任何远程控制面**。
    ///
    /// **内部访问器**（v3 P3 方法面收敛）：凭证签发逻辑在 negotiator/control
    /// 域直连本方法，不再作为 Kernel 门面 `pub fn`。
    pub(crate) fn create_share_token(
        &self,
        session_id: &str,
        media: Vec<MediaKind>,
        ttl: Duration,
    ) -> Result<ShareToken> {
        if !self.has_session(session_id) {
            return Err(Error::SessionNotFound(session_id.to_string()));
        }
        let now = now_secs();
        // 惰性清理过期凭证，保持签发表有界
        let mut tokens = self.share_tokens.lock_poisoned();
        tokens.retain(|_, t| !t.is_expired(now));
        let token = ShareToken {
            v: ShareToken::VERSION,
            stream_id: StreamId::new(session_id),
            pin: random_pin(session_id),
            expires_at: now.saturating_add(ttl.as_secs()),
            media,
        };
        tokens.insert(Id::from(session_id), token.clone());
        Ok(token)
    }

    /// 校验凭证：已签发 + 未过期 + 与签发时逐字一致（防篡改 / 重放）。
    pub fn verify_share_token(&self, token: &ShareToken) -> Result<()> {
        let tokens = self.share_tokens.lock_poisoned();
        let stored = tokens
            .get(&Id::from(token.stream_id.as_str()))
            .ok_or_else(|| {
                Error::Token(format!("凭证无效：会话 {} 未签发凭证", token.stream_id))
            })?;
        if stored != token {
            return Err(Error::Token(
                "凭证无效：与签发时不符（可能被篡改或重放）".into(),
            ));
        }
        if stored.is_expired(now_secs()) {
            return Err(Error::Token("凭证已过期".into()));
        }
        Ok(())
    }

    /// 数据面凭证校验器（读本内核签发表；注入受控中继用）。
    ///
    /// 与 [`Kernel::attach_data_plane`] 配套的数据面接线原语：测试 / 独立接线
    /// 方可把校验器注入后端而**不**整体接线（如凭证式跨设备推流闭环）；
    /// 常规路径经 `attach_data_plane` 内部注入，壳层无需直调。
    pub fn token_validator(&self) -> Arc<dyn crate::relay::ShareTokenValidator> {
        Arc::new(KernelTokenValidator {
            tokens: self.share_tokens.clone(),
        })
    }

    // -----------------------------------------------------------------------
    // 跨设备凭证（B2：接收手机麦克风）
    // -----------------------------------------------------------------------

    /// 通用凭证签发（媒体 / 标题可定制；协商端点与手动路径共用）。
    ///
    /// **内部访问器**（v3 P3 方法面收敛）：凭证签发逻辑在 negotiator 域直连
    /// （`file_xfer` 测试亦经此路径），不再作为 Kernel 门面 `pub fn`。
    pub(crate) fn issue_share_token_for(
        &self,
        title: String,
        media: Vec<MediaKind>,
        ttl_secs: Option<u64>,
    ) -> Result<ShareTokenView> {
        let prefs = SessionPrefs {
            title,
            ..Default::default()
        };
        // 会话 id 由本方法（协商域）签发（sess-N 随机，与旧 create_session
        // 语义一致）；构建核心走共享 `build_session`。
        let id = StreamId::new(format!(
            "sess-{:x}",
            self.next_id.fetch_add(1, Ordering::Relaxed)
        ));
        let session = self.build_session(id, "local", &["local".into()], &prefs)?;
        let ttl = Duration::from_secs(ttl_secs.unwrap_or(600));
        let token = self.create_share_token(&session.id, media, ttl)?;
        Ok(ShareTokenView {
            token: token.to_token_string(),
            stream_id: token.stream_id,
            pin: token.pin,
            expires_at: token.expires_at,
        })
    }
}

/// 数据面接入凭证校验器：读内核签发表，校验"存在 + 未过期 + 逐字一致"。
pub(crate) struct KernelTokenValidator {
    pub(crate) tokens: Arc<std::sync::Mutex<std::collections::HashMap<Id, ShareToken>>>,
}

impl crate::relay::ShareTokenValidator for KernelTokenValidator {
    fn validate(&self, token: &ShareToken) -> bool {
        let tokens = self.tokens.lock_poisoned();
        let Some(stored) = tokens.get(&Id::from(token.stream_id.as_str())) else {
            return false;
        };
        stored == token && !stored.is_expired(now_secs())
    }
}
