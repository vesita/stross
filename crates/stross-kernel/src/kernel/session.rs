//! 会话管理：会话拓扑与协商结果。

use std::collections::HashMap;
use std::sync::Mutex;

use serde::Serialize;

use stross_proto::message::{CodecId, ReliabilityProfile, RoutePath, StreamId, TransportId};

use super::super::lock::MutexExt;
use crate::error::{Error, Result};
use crate::kernel::id::Id;

/// 会话协商结果（阶段 1 起由 Offer/Answer 填充）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Negotiated {
    pub transport: TransportId,
    pub codec: CodecId,
    pub profile: ReliabilityProfile,
}

/// 一条「从 A 推送到 B（可多个）」的互联会话。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: StreamId,
    /// 会话标题（接收端建会话时填写，如「手机麦克风」；UI 展示用）。
    pub title: String,
    pub source: String,
    pub sinks: Vec<String>,
    pub path: RoutePath,
    pub negotiated: Negotiated,
    /// 会话是否启用访问码（PIN）——控制操作需先 [`crate::kernel::Kernel::authorize`]。
    pub requires_pin: bool,
    /// 控制面是否已通过鉴权（内部状态，不序列化）。
    #[serde(skip)]
    pub(super) authorized: bool,
}

/// 会话创建偏好。
#[derive(Debug, Clone, Default)]
pub struct SessionPrefs {
    pub profile: ReliabilityProfile,
    pub preferred_transport: Option<TransportId>,
    /// 会话访问码（PIN，可选）：设置后控制操作（route / teardown）需先
    /// [`crate::kernel::Kernel::authorize`]（设计文档 §7 会话级访问码）。
    pub access_code: Option<String>,
    /// 会话标题（原 CreateSession.title 死字段；随会话存储供 UI 展示）。
    pub title: String,
}

impl Session {
    /// 控制操作前的鉴权门禁：启用访问码且未通过 [`crate::kernel::Kernel::authorize`] → 拒绝。
    pub(super) const fn require_authorized(&self) -> Result<()> {
        if self.requires_pin && !self.authorized {
            return Err(Error::PinRequired);
        }
        Ok(())
    }

    /// 标记已鉴权（仅 [`crate::kernel::Kernel::authorize`] 调用）。
    pub(super) const fn mark_authorized(&mut self) {
        self.authorized = true;
    }
}

/// 会话管理：会话拓扑与协商结果。
#[derive(Default)]
pub(crate) struct SessionManager {
    sessions: Mutex<HashMap<Id, Session>>,
}

impl SessionManager {
    /// 会话是否存在。
    pub(super) fn contains(&self, id: &Id) -> bool {
        self.sessions.lock_poisoned().contains_key(id)
    }

    /// 会话快照（不存在 → `None`）。
    pub(super) fn get(&self, id: &Id) -> Option<Session> {
        self.sessions.lock_poisoned().get(id).cloned()
    }

    /// 登记会话。
    pub(super) fn insert(&self, session: Session) {
        // `Session.id` 是线序/壳层可见的 String；登记时经 Id 定为内部 key。
        self.sessions
            .lock_poisoned()
            .insert(Id::from(session.id.as_str()), session);
    }

    /// 移除会话（返回被移除项；不存在 → `None`）。
    pub(super) fn remove(&self, id: &Id) -> Option<Session> {
        self.sessions.lock_poisoned().remove(id)
    }

    /// 全量快照（按 id 排序）。
    pub(super) fn snapshot(&self) -> Vec<Session> {
        let guard = self.sessions.lock_poisoned();
        let mut v: Vec<_> = guard.values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    /// 控制操作前的鉴权门禁：会话必须存在且已授权（F2.5 / 设计文档 §7）。
    pub(super) fn require_authorized(&self, id: &Id) -> Result<()> {
        let guard = self.sessions.lock_poisoned();
        let s = guard
            .get(id)
            .ok_or_else(|| Error::SessionNotFound(id.to_string()))?;
        s.require_authorized()
    }

    /// 改道：校验鉴权后更新传输路径（F2.3 会话内动态改道）。
    pub(super) fn route(&self, id: &Id, path: RoutePath) -> Result<()> {
        let mut guard = self.sessions.lock_poisoned();
        let s = guard
            .get_mut(id)
            .ok_or_else(|| Error::SessionNotFound(id.to_string()))?;
        s.require_authorized()?;
        s.path = path;
        Ok(())
    }

    /// 标记已鉴权（访问码校验成功后调用）。
    pub(super) fn mark_authorized(&self, id: &Id) -> Result<()> {
        let mut guard = self.sessions.lock_poisoned();
        let s = guard
            .get_mut(id)
            .ok_or_else(|| Error::SessionNotFound(id.to_string()))?;
        s.mark_authorized();
        Ok(())
    }
}

/// 路由：传输方向选择策略。
pub(super) struct Router;

impl Router {
    /// 默认路径：单接收端直连；多接收端组播；无接收端经本机中继兜底。
    pub(super) fn default_path(sinks: &[String]) -> RoutePath {
        match sinks {
            [one] => RoutePath::Direct { node: one.clone() },
            many => RoutePath::Mesh {
                nodes: many.to_vec(),
            },
        }
    }
}
