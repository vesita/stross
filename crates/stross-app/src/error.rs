//! 应用级统一错误模型。
//!
//! 把散落的 `Result<_, String>` 收口为带语义的 [`Error`] 枚举：
//! 内核（会话 / 鉴权 / 凭证）、数据面、观看链路各自有独立变体，
//! 业务校验类错误进 [`Error::Message`]，基础设施类错误统一
//! 经 `#[from] anyhow::Error` 进 [`Error::Internal`]。
//!
//! 用户可见文本只在边界转换一次：Tauri 命令用
//! [`Error::to_user_string`]，控制面 [`crate::CtrlResponse`] 同理；
//! 内部一律使用类型化 [`Result<T>`]，不携带格式化的字符串。

use crate::kernel::AuthError;

/// 应用级错误。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// 通用业务错误（用户可见文本已就绪）。
    #[error("{0}")]
    Message(String),
    /// 会话不存在。
    #[error("会话 {0} 不存在")]
    SessionNotFound(String),
    /// 会话启用访问码（PIN）但尚未通过鉴权。
    #[error("会话需要访问码（PIN），请先 authorize")]
    PinRequired,
    /// 访问码错误。
    #[error("访问码错误")]
    PinMismatch,
    /// 数据面操作失败（流预授权 / 撤销）。
    #[error("数据面操作失败: {0}")]
    DataPlane(String),
    /// 接入凭证无效（未签发 / 篡改 / 过期）。
    #[error("{0}")]
    Token(String),
    /// 链路失败（观看直连 / 级联代理）。
    #[error("{0}")]
    Link(String),
    /// 内部错误（基础设施层 anyhow 错误）。
    #[error("{0}")]
    Internal(#[from] anyhow::Error),
}

/// 应用级结果别名。
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// 用户可见错误文本（边界转换：Tauri 命令 / CtrlResponse）。
    pub fn to_user_string(&self) -> String {
        self.to_string()
    }
}

impl From<AuthError> for Error {
    fn from(e: AuthError) -> Self {
        match e {
            AuthError::CodeRequired => Error::PinRequired,
            AuthError::CodeMismatch => Error::PinMismatch,
        }
    }
}
