//! 数据面后端：内核（控制面）驱动中继（数据面）的接口与实现。
//!
//! 需求 F2.2「先会话后传输」：推收关系确定后（[`Kernel::create_session`]）才
//! 允许对应流接入（受控中继的 `authorize_stream`）；流的起止与观看人数变化
//! 经 [`RelayEvent`] 上报内核，转发为 [`KernelEvent`]（见 [`super::Kernel`]）。
//!
//! 设计文档 `docs/requirements.md` §7：中继 = 受内核驱动的数据面后端（主从关系）。

use std::sync::Arc;
use tokio::sync::broadcast;

use crate::relay::{RelayEvent, RelayHandle, RelayState, ShareTokenValidator};

use crate::error::Result;

/// 数据面后端：内核通过它预授权 / 撤销流接入，并订阅流生命周期事件。
///
/// 预授权 / 撤销是纯内存标记操作（受控中继按 id 放行 Hello），无需跨 await；
/// 同步签名让会话创建 / 拆除保持同步，也避免调用方持锁跨 await。
pub trait DataPlaneBackend: Send + Sync + 'static {
    /// 预授权一个会话 id（受控中继据此允许对应 Hello 接入）。
    fn authorize_stream(&self, session_id: &str) -> Result<()>;
    /// 撤销预授权（会话拆除时调用）。
    fn revoke_stream(&self, session_id: &str) -> Result<()>;
    /// 数据面事件订阅（StreamStarted / StreamEnded / WatchersChanged）。
    fn events(&self) -> broadcast::Receiver<RelayEvent>;
    /// 注入接入凭证校验器（B 阶段跨设备推流；默认不注入 = 行为与现状一致）。
    fn set_share_token_validator(&self, _validator: Arc<dyn ShareTokenValidator>) {}
}

/// 内嵌中继适配器：把中继共享状态包装为数据面后端（本机同进程闭环）。
///
/// 只持有 [`RelayState`]（可克隆、全部 Arc 字段），不占用 [`RelayHandle`]
/// （句柄仍归调用方，负责 stop）。
pub struct RelayDataPlane {
    state: RelayState,
}

impl RelayDataPlane {
    /// 包装一个（受控模式启动的）中继句柄。
    pub fn new(handle: &RelayHandle) -> Self {
        Self {
            state: handle.state(),
        }
    }
}

impl DataPlaneBackend for RelayDataPlane {
    fn authorize_stream(&self, session_id: &str) -> Result<()> {
        self.state.authorize_stream(session_id);
        Ok(())
    }

    fn revoke_stream(&self, session_id: &str) -> Result<()> {
        self.state.revoke_stream(session_id);
        Ok(())
    }

    fn events(&self) -> broadcast::Receiver<RelayEvent> {
        self.state.subscribe_events()
    }

    fn set_share_token_validator(&self, validator: Arc<dyn ShareTokenValidator>) {
        self.state.set_token_validator(Some(validator));
    }
}
