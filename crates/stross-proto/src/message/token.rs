//! 一次性接入凭证（跨设备推流，见 docs/iteration-plan.md B0/B1）。
//!
//! 场景：**接收端**（如电脑）主动建会话并签发凭证，**推流端**（如手机）出示
//! 凭证直接向接收端的受控中继推流——受控中继在预授权（[`Kernel` 数据面]）
//! 之外接受"凭证匹配"作为接入凭据，实现跨设备推流而**不开放任何远程控制面**
//! （D7：控制面仍仅回环）。
//!
//! 安全模型：凭证是**一次性密码本**——`pin` 为签发时生成的随机串，服务端
//! 存储签发时的完整凭证，推流端出示必须逐字匹配且未过期。凭证经二维码 / 短码
//! 展示（参考 QuicMic 的接入模式 + F2.5 会话级 PIN 语义）；不进日志、不进
//! mDNS TXT、不进进程参数。
//!
//! [`Kernel`]: https://docs.rs/stross-app/latest/stross_app/kernel/struct.Kernel.html

use serde::{Deserialize, Serialize};

use super::ids::MediaKind;

/// 一次性接入凭证。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareToken {
    /// 描述版本（自描述演进；当前 [`ShareToken::VERSION`]）。
    pub v: u8,
    /// 接收端内核签发的会话 id（D4：与 stream_id 合一）；推流 Hello 必须携带同一 id。
    pub stream_id: String,
    /// 签发时生成的随机 PIN（一次性；服务端存储为准，防重放/篡改）。
    pub pin: String,
    /// 过期时间（Unix 秒）；过期后中继拒绝接入。
    pub expires_at: u64,
    /// 本次共享的媒体类型（如 `mic`；供接收端 UI 展示 / 校验）。
    pub media: Vec<MediaKind>,
}

impl ShareToken {
    /// 当前凭证版本。
    pub const VERSION: u8 = 1;

    /// 编码为字符串（JSON；二维码 / 短码友好，与 DiscoveryInfo 单 key JSON 同风格）。
    pub fn to_token_string(&self) -> String {
        serde_json::to_string(self).expect("ShareToken 序列化不应失败")
    }

    /// 从字符串解码；缺失 / 非法返回 `None`（调用方拒绝接入）。
    pub fn from_token_string(s: &str) -> Option<Self> {
        serde_json::from_str(s).ok()
    }

    /// 是否已过期（`now_secs` 为当前 Unix 秒）。
    pub fn is_expired(&self, now_secs: u64) -> bool {
        self.expires_at <= now_secs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token() -> ShareToken {
        ShareToken {
            v: ShareToken::VERSION,
            stream_id: "sess-1".into(),
            pin: "483920".into(),
            expires_at: 1_800_000_000,
            media: vec![MediaKind::Mic, MediaKind::SystemAudio],
        }
    }

    #[test]
    fn share_token_roundtrip() {
        let t = token();
        let s = t.to_token_string();
        assert!(s.contains("\"streamId\":\"sess-1\""), "wire: {s}");
        assert_eq!(ShareToken::from_token_string(&s).unwrap(), t);
    }

    #[test]
    fn share_token_tolerant() {
        // 坏 JSON → None
        assert_eq!(ShareToken::from_token_string("{oops"), None);
        // 未知枚举值（media）→ 解码失败
        assert!(
            ShareToken::from_token_string(
                r#"{"v":1,"streamId":"s","pin":"1","expiresAt":1,"media":["hacker"]}"#
            )
            .is_none()
        );
    }

    #[test]
    fn share_token_expiry() {
        let mut t = token();
        t.expires_at = 100;
        assert!(t.is_expired(100), "边界：等于 expires_at 即过期");
        assert!(t.is_expired(101));
        assert!(!t.is_expired(99));
    }
}
