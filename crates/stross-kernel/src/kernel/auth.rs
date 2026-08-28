//! 控制面鉴权（设计文档 §7）。

use super::super::lock::MutexExt;
use std::collections::HashMap;
use std::sync::Mutex;

/// 会话鉴权错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// 会话设置了访问码但未提供。
    CodeRequired,
    /// 访问码不匹配。
    CodeMismatch,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::CodeRequired => write!(f, "会话需要访问码（PIN）"),
            AuthError::CodeMismatch => write!(f, "访问码错误"),
        }
    }
}

impl std::error::Error for AuthError {}

/// 控制面鉴权策略。
///
/// 阶段 2 内置 [`PinAuthPolicy`]；远期可换 WASM 策略插件（Extism）而不动内核。
pub trait AuthPolicy: Send + Sync {
    /// 校验访问码；返回 `Ok` 放行。
    fn authorize(&self, session_id: &str, access_code: Option<&str>) -> Result<(), AuthError>;
    /// 设置/清除会话访问码（`None` = 清除）。默认无操作（无状态策略）。
    fn set_code(&self, _session_id: &str, _code: Option<&str>) {}
}

/// 内置 PIN 策略：会话创建者设置访问码，控制操作前必须通过 [`AuthPolicy::authorize`]。
#[derive(Default)]
pub struct PinAuthPolicy {
    pins: Mutex<HashMap<String, String>>,
}

impl AuthPolicy for PinAuthPolicy {
    fn authorize(&self, session_id: &str, access_code: Option<&str>) -> Result<(), AuthError> {
        let pin = self.pins.lock_poisoned().get(session_id).cloned();
        match pin {
            None => Ok(()), // 会话无访问码，放行
            Some(pin) => match access_code {
                None => Err(AuthError::CodeRequired),
                Some(code) if code == pin => Ok(()),
                Some(_) => Err(AuthError::CodeMismatch),
            },
        }
    }

    fn set_code(&self, session_id: &str, code: Option<&str>) {
        let mut pins = self.pins.lock_poisoned();
        match code {
            Some(code) => {
                pins.insert(session_id.to_string(), code.to_string());
            }
            None => {
                pins.remove(session_id);
            }
        }
    }
}
