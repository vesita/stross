//! mDNS 发现能力引导（F1.2：单 key JSON，新增字段零维护）。

use serde::{Deserialize, Serialize};

use super::ids::{CodecId, MediaKind, RoleId, TransportId};

/// mDNS TXT 中承载 [`DiscoveryInfo`] 的固定 key。
pub const TXT_KEY_DISCOVERY: &str = "stross";

/// 发现能力引导载荷（需求 F1.2；设计见 docs/requirements.md D7 旁注）。
///
/// 整个描述序列化进**单个 TXT key `stross`**（JSON）：注册侧 `to_txt` 一键编码、
/// 浏览侧 `from_txt` 一键解码——**新增字段零维护**，枚举经 serde 保持 wire
/// 格式稳定（lowercase，与旧逗号串时代一致）。设备名同时出现在 mDNS 实例名中
/// （第三方浏览器可读）；不再逐 key 手工拼 `name/roles/transports`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryInfo {
    /// 描述版本（自描述演进；当前 [`DiscoveryInfo::VERSION`]）。
    pub v: u8,
    /// 设备名。
    pub name: String,
    /// 角色。
    pub roles: Vec<RoleId>,
    /// 可共享的媒体类型。
    pub media: Vec<MediaKind>,
    /// 支持的传输。
    pub transports: Vec<TransportId>,
    /// 支持的编解码。
    pub codecs: Vec<CodecId>,
}

impl DiscoveryInfo {
    /// 当前描述版本。
    pub const VERSION: u8 = 1;

    /// 中继 / 本机锚点广播的默认能力描述（roles = Relay/Sender/Viewer、
    /// transports = ws/webrtc/srt/quic、codecs = h264/aac 固定；
    /// `media` 按设备实际可共享项传入）。
    ///
    /// 统一 4 处广播点（app 锚点 / stross-relay / cli relay / GUI relay-only）
    /// 的构造，避免重复实现与字段漂移。
    pub fn relay_default(name: impl Into<String>, media: Vec<MediaKind>) -> Self {
        Self {
            v: Self::VERSION,
            name: name.into(),
            roles: vec![RoleId::Relay, RoleId::Sender, RoleId::Viewer],
            media,
            transports: vec![
                TransportId::Ws,
                TransportId::WebRtc,
                TransportId::Srt,
                TransportId::Quic,
            ],
            codecs: vec![CodecId::H264, CodecId::Aac],
        }
    }

    /// 编码为 mDNS TXT 条目（单 key）。
    pub fn to_txt(&self) -> Vec<(String, String)> {
        vec![(
            TXT_KEY_DISCOVERY.to_string(),
            serde_json::to_string(self).expect("DiscoveryInfo 序列化不应失败"),
        )]
    }

    /// 从 mDNS TXT 条目解码；缺失 / 非法返回 `None`（调用方回退默认值）。
    pub fn from_txt(txt: &[(String, String)]) -> Option<Self> {
        let json = txt.iter().find(|(k, _)| k == TXT_KEY_DISCOVERY)?.1.as_str();
        serde_json::from_str(json).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_info_txt_roundtrip() {
        let info = DiscoveryInfo {
            v: DiscoveryInfo::VERSION,
            name: "卧室电脑".into(),
            roles: vec![RoleId::Relay, RoleId::Sender, RoleId::Viewer],
            media: vec![MediaKind::Screen, MediaKind::Mic],
            transports: vec![TransportId::Ws, TransportId::WebRtc],
            codecs: vec![CodecId::H264, CodecId::Aac],
        };
        let txt = info.to_txt();
        assert_eq!(txt.len(), 1, "单 key 承载全部能力");
        assert_eq!(txt[0].0, TXT_KEY_DISCOVERY);
        assert!(
            txt[0]
                .1
                .contains("\"roles\":[\"relay\",\"sender\",\"viewer\"]"),
            "wire: {}",
            txt[0].1
        );
        let back = DiscoveryInfo::from_txt(&txt).expect("roundtrip 解码");
        assert_eq!(info, back);
    }

    #[test]
    fn discovery_info_from_txt_tolerant() {
        // 缺失 key → None（调用方回退默认）
        assert_eq!(DiscoveryInfo::from_txt(&[]), None);
        assert_eq!(
            DiscoveryInfo::from_txt(&[("name".into(), "x".into())]),
            None
        );
        // 坏 JSON → None
        assert_eq!(
            DiscoveryInfo::from_txt(&[(TXT_KEY_DISCOVERY.into(), "{oops".into())]),
            None
        );
        // 未知枚举值 → 解码失败（枚举穷尽，未知值拒绝）
        assert_eq!(
            DiscoveryInfo::from_txt(&[(
                TXT_KEY_DISCOVERY.into(),
                r#"{"v":1,"name":"x","roles":["hacker"]}"#.into()
            )]),
            None
        );
    }
}
