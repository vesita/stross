//! 内核级统一错误模型。
//!
//! 把散落的 `Result<_, String>` 收口为带语义的 [`Error`] 枚举：
//! 会话 / 鉴权 / 凭证 / 数据面 / 观看链路各自有独立变体，业务校验类错误进
//! [`Error::Message`]，基础设施类错误统一经 `#[from] anyhow::Error` 进
//! [`Error::Internal`]。观看链路与中继数据面的细粒度错误（[`WatchError`] /
//! [`RelayOpError`]）也在此定义。
//!
//! 用户可见文本只在边界转换一次：Tauri 命令用 [`Error::to_user_string`]，
//! 控制面 [`crate::CtrlResponse`] 同理；内部一律使用类型化 [`Result<T>`]，
//! 不携带格式化的字符串。

use crate::kernel::auth::AuthError;

/// 内核级错误。
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

/// 内核级结果别名。
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
            AuthError::CodeRequired => Self::PinRequired,
            AuthError::CodeMismatch => Self::PinMismatch,
        }
    }
}

/// 观看连接错误（[`crate::watch::connect_watch`]）。
#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    /// 地址无法解析（未知 scheme / 缺端口）。
    #[error("无法解析中继地址: {0}")]
    InvalidUrl(String),
    /// 传输层拨号失败。
    #[error("连接中继失败: {0}")]
    Connect(String),
    /// 发送 Watch 请求失败（SRT/QUIC 带内声明）。
    #[error("发送 Watch 请求失败: {0}")]
    SendWatch(String),
    /// 中继拒绝观看（返回 Error 控制消息）。
    #[error("中继拒绝: {0}")]
    Rejected(String),
    /// 等待 Ready 回执失败 / 异常。
    #[error("等待中继就绪失败: {0}")]
    WaitReady(String),
    /// 中继在就绪前关闭连接。
    #[error("中继连接已关闭")]
    Closed,
}

/// 中继数据面操作错误（[`crate::relay::RelayState::start_proxy`]）。
#[derive(Debug, thiserror::Error)]
pub enum RelayOpError {
    /// 本地已有同名代理流。
    #[error("本地已有代理流 {0}")]
    ProxyExists(String),
    /// 本地已有同名流（推流或代理）。
    #[error("本地已有流 {0}（推流或代理）")]
    StreamExists(String),
}
