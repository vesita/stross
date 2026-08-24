//! 会话管理：会话拓扑与协商结果。

use std::collections::HashMap;
use std::sync::Mutex;

use serde::Serialize;

use stross_proto::message::{CodecId, ReliabilityProfile, RoutePath, TransportId};

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
    pub id: String,
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
}

impl Session {
    /// 控制操作前的鉴权门禁：启用访问码且未通过 [`crate::kernel::Kernel::authorize`] → 拒绝。
    pub(super) fn require_authorized(&self) -> Result<(), String> {
        if self.requires_pin && !self.authorized {
            return Err("会话需要访问码（PIN），请先 authorize".into());
        }
        Ok(())
    }

    /// 标记已鉴权（仅 [`crate::kernel::Kernel::authorize`] 调用）。
    pub(super) fn mark_authorized(&mut self) {
        self.authorized = true;
    }
}

/// 会话管理：会话拓扑与协商结果。
#[derive(Default)]
pub(super) struct SessionManager {
    pub(super) sessions: Mutex<HashMap<String, Session>>,
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
