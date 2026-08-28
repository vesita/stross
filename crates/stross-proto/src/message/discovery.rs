//! mDNS 发现能力引导（F1.2：单 key JSON，新增字段零维护）。

use serde::{Deserialize, Serialize};

use super::endpoint::DeviceSummary;
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
    /// 设备清单摘要（v2，见 [`DeviceSummary`]）：只带 id/kind/name/是否已公开，
    /// 协议/可见性/状态等详情走 L2 `/api/endpoints` 拉取（TXT 体积受限；
    /// 多 key 广播时设备摘要走 `dev.<n>` key，base 里恒为空 → 序列化省略）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<DeviceSummary>,
}

impl DiscoveryInfo {
    /// 当前描述版本（v2：新增 `devices` 设备清单摘要；v1 解析器忽略未知字段）。
    pub const VERSION: u8 = 2;

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
            devices: Vec::new(),
        }
    }

    /// 追加设备清单摘要（P1：锚定广播时快照当前 publish 状态；relay-only
    /// 进程不调用，广播空清单）。
    pub fn with_devices(mut self, devices: Vec<DeviceSummary>) -> Self {
        self.devices = devices;
        self
    }

    /// 编码为 mDNS TXT 条目（**多 key，方案 b，docs/endpoint-model.md §3.4**）。
    ///
    /// mDNS TXT 每条 character-string ≤ 255B（RFC 1035 §3.3，mdns-sd 强校验；
    /// 实测整包 JSON（base + 3 台设备摘要）达 449B 直接广播失败）。
    /// 拆分：基础能力走 `stross` key（≈200B）；每台设备各占 `dev.<n>` key。
    /// v1 端只读 `stross` key（忽略未知 key），新端合并两处——双向兼容。
    pub fn to_txt(&self) -> Vec<(String, String)> {
        let mut out = Vec::with_capacity(1 + self.devices.len());
        let mut base = self.clone();
        base.devices = Vec::new();
        let base_json = serde_json::to_string(&base).expect("DiscoveryInfo 序列化不应失败");
        debug_assert!(
            base_json.len() <= 255,
            "base TXT 超 255B（{len}B）：{base_json}",
            len = base_json.len()
        );
        out.push((TXT_KEY_DISCOVERY.to_string(), base_json));
        for (i, d) in self.devices.iter().enumerate() {
            let dev_json = serde_json::to_string(d).expect("设备摘要序列化不应失败");
            debug_assert!(
                dev_json.len() <= 255,
                "设备 TXT 超 255B（{len}B）: {dev_json}",
                len = dev_json.len()
            );
            out.push((format!("dev.{i}"), dev_json));
        }
        out
    }

    /// 从 mDNS TXT 条目解码（多 key 合并 + 单 key 兼容）；缺失 / 非法返回 `None`
    /// （调用方回退默认值）。
    pub fn from_txt(txt: &[(String, String)]) -> Option<Self> {
        let mut info: DiscoveryInfo =
            serde_json::from_str(txt.iter().find(|(k, _)| k == TXT_KEY_DISCOVERY)?.1.as_str())
                .ok()?;
        // 多 key 形态：`dev.<n>` → 按序号合并进 devices（单 key 旧广播无此键，
        // devices 保持 base 内嵌值）
        let mut devs: Vec<(usize, DeviceSummary)> = Vec::new();
        for (k, v) in txt {
            if let Some(idx) = k.strip_prefix("dev.")
                && let Ok(d) = serde_json::from_str::<DeviceSummary>(v)
                && let Ok(n) = idx.parse::<usize>()
            {
                devs.push((n, d));
            }
        }
        devs.sort_by_key(|(n, _)| *n);
        if !devs.is_empty() {
            info.devices = devs.into_iter().map(|(_, d)| d).collect();
        }
        Some(info)
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
            devices: vec![DeviceSummary {
                device_id: "mic:builtin".into(),
                kind: MediaKind::Mic,
                name: "麦克风".into(),
                published: true,
            }],
        };
        let txt = info.to_txt();
        // 多 key（§3.4 方案 b）：base + 每设备一个 key，均 ≤255B
        assert_eq!(txt.len(), 2, "base + 设备各占一个 key");
        assert_eq!(txt[0].0, TXT_KEY_DISCOVERY);
        assert!(txt[0].1.len() <= 255, "base TXT 应在 255B 内");
        assert!(
            txt[0]
                .1
                .contains("\"roles\":[\"relay\",\"sender\",\"viewer\"]"),
            "wire: {}",
            txt[0].1
        );
        assert!(!txt[0].1.contains("\"devices\""), "设备摘要移出 base key");
        assert_eq!(txt[1].0, "dev.0");
        assert!(txt[1].1.len() <= 255, "设备 TXT 应在 255B 内");
        assert!(txt[1].1.contains("\"deviceId\":\"mic:builtin\""));
        let back = DiscoveryInfo::from_txt(&txt).expect("roundtrip 解码");
        assert_eq!(info, back);
    }

    #[test]
    fn txt_multi_key_fits_255_per_platform_summary() {
        // 实测回归（§3.4/§11.1）：整包 449B 超限广播失败；多 key 后每 key 必须
        // ≤255B。用桌面三台设备的真实摘要验证。
        let devices = vec![
            DeviceSummary {
                device_id: "screen:0".into(),
                kind: MediaKind::Screen,
                name: "屏幕".into(),
                published: true,
            },
            DeviceSummary {
                device_id: "mic:builtin".into(),
                kind: MediaKind::Mic,
                name: "麦克风".into(),
                published: false,
            },
            DeviceSummary {
                device_id: "sysaudio:builtin".into(),
                kind: MediaKind::SystemAudio,
                name: "系统声音".into(),
                published: false,
            },
        ];
        let info = DiscoveryInfo::relay_default(
            "Stross 设备",
            vec![
                MediaKind::Screen,
                MediaKind::Camera,
                MediaKind::Mic,
                MediaKind::SystemAudio,
            ],
        )
        .with_devices(devices);
        let txt = info.to_txt();
        for (k, v) in &txt {
            assert!(v.len() <= 255, "{k} 超 255B（{}B）: {v}", v.len());
        }
        let back = DiscoveryInfo::from_txt(&txt).expect("多 key roundtrip");
        assert_eq!(back.devices.len(), 3, "设备按序合并回");
        assert_eq!(back.devices[0].device_id, "screen:0");
        assert_eq!(back.devices[1].kind, MediaKind::Mic);
        assert!(back.devices[0].published);
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
        // v1 载荷（无 devices 字段）→ 兼容解析，devices 默认空
        let v1 = r#"{"v":1,"name":"x","roles":["relay"],"media":[],"transports":[],"codecs":[]}"#;
        let back =
            DiscoveryInfo::from_txt(&[(TXT_KEY_DISCOVERY.into(), v1.into())]).expect("v1 兼容解析");
        assert!(back.devices.is_empty());
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
