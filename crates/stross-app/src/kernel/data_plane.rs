//! 数据面后端：内核（控制面）驱动中继（数据面）的接口与实现。
//!
//! 需求 F2.2「先会话后传输」：推收关系确定后（[`Kernel::create_session`]）才
//! 允许对应流接入（受控中继的 `authorize_stream`）；流的起止与观看人数变化
//! 经 [`RelayEvent`] 上报内核，转发为 [`KernelEvent`]（见 [`super::Kernel`]）。
//!
//! 设计文档 `docs/requirements.md` §7：中继 = 受内核驱动的数据面后端（主从关系）。

use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::broadcast;

use stross_core::relay::{RelayEvent, RelayHandle, RelayState, ShareTokenValidator};

/// 数据面后端：内核通过它预授权 / 撤销流接入，并订阅流生命周期事件。
#[async_trait]
pub trait DataPlaneBackend: Send + Sync + 'static {
    /// 预授权一个会话 id（受控中继据此允许对应 Hello 接入）。
    async fn authorize_stream(&self, session_id: &str) -> Result<(), String>;
    /// 撤销预授权（会话拆除时调用）。
    async fn revoke_stream(&self, session_id: &str) -> Result<(), String>;
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

#[async_trait]
impl DataPlaneBackend for RelayDataPlane {
    async fn authorize_stream(&self, session_id: &str) -> Result<(), String> {
        self.state.authorize_stream(session_id);
        Ok(())
    }

    async fn revoke_stream(&self, session_id: &str) -> Result<(), String> {
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
