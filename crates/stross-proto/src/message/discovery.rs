//! mDNS 发现能力引导（F1.2：单 key JSON，新增字段零维护）。

use serde::{Deserialize, Serialize};

use super::endpoint::EndpointSummary;
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
    /// 端点清单摘要（v3，见 [`EndpointSummary`]）：只带 id/kind/name/是否可挂载/
    /// 是否已通告，协议/可见性/状态等详情走 L2 `/api/endpoints` 拉取（TXT 体积
    /// 受限；多 key 广播时端点摘要走 `ep.<n>` key，base 里恒为空 → 序列化省略）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub endpoints: Vec<EndpointSummary>,
}

impl DiscoveryInfo {
    /// 当前描述版本（v3：单层端点模型——`devices` 摘要升级为 `endpoints`
    /// 摘要，携带 `available`（能否挂载）；v2 及以前为设备/端点两层模型，
    /// 与 v3 不兼容，需全端同步升级）。
    pub const VERSION: u8 = 3;

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
            endpoints: Vec::new(),
        }
    }

    /// 追加端点清单摘要（锚定广播时快照当前端点状态；relay-only 进程不调用，
    /// 广播空清单）。
    pub fn with_endpoints(mut self, endpoints: Vec<EndpointSummary>) -> Self {
        self.endpoints = endpoints;
        self
    }

    /// 编码为 mDNS TXT 条目（**多 key，方案 b，docs/endpoint-model-v2.md**）。
    ///
    /// mDNS TXT 每条 character-string ≤ 255B（RFC 1035 §3.3，mdns-sd 强校验；
    /// 实测整包 JSON（base + 3 台设备摘要）达 449B 直接广播失败）。
    /// 拆分：基础能力走 `stross` key（≈200B）；每个端点各占 `ep.<n>` key。
    pub fn to_txt(&self) -> Vec<(String, String)> {
        let mut out = Vec::with_capacity(1 + self.endpoints.len());
        let mut base = self.clone();
        base.endpoints = Vec::new();
        let base_json = serde_json::to_string(&base).expect("DiscoveryInfo 序列化不应失败");
        debug_assert!(
            base_json.len() <= 255,
            "base TXT 超 255B（{len}B）：{base_json}",
            len = base_json.len()
        );
        out.push((TXT_KEY_DISCOVERY.to_string(), base_json));
        for (i, e) in self.endpoints.iter().enumerate() {
            let ep_json = serde_json::to_string(e).expect("端点摘要序列化不应失败");
            debug_assert!(
                ep_json.len() <= 255,
                "端点 TXT 超 255B（{len}B）: {ep_json}",
                len = ep_json.len()
            );
            out.push((format!("ep.{i}"), ep_json));
        }
        out
    }

    /// 从 mDNS TXT 条目解码（多 key 合并 + 单 key 兼容）；缺失 / 非法返回 `None`
    /// （调用方回退默认值）。
    pub fn from_txt(txt: &[(String, String)]) -> Option<Self> {
        let mut info: Self =
            serde_json::from_str(txt.iter().find(|(k, _)| k == TXT_KEY_DISCOVERY)?.1.as_str())
                .ok()?;
        // 多 key 形态：`ep.<n>` → 按序号合并进 endpoints（单 key 旧广播无此键，
        // endpoints 保持 base 内嵌值）
        let mut eps: Vec<(usize, EndpointSummary)> = Vec::new();
        for (k, v) in txt {
            if let Some(idx) = k.strip_prefix("ep.")
                && let Ok(e) = serde_json::from_str::<EndpointSummary>(v)
                && let Ok(n) = idx.parse::<usize>()
            {
                eps.push((n, e));
            }
        }
        eps.sort_by_key(|(n, _)| *n);
        if !eps.is_empty() {
            info.endpoints = eps.into_iter().map(|(_, e)| e).collect();
        }
        Some(info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_summary() -> EndpointSummary {
        EndpointSummary {
            endpoint_id: "mic:builtin".into(),
            kind: MediaKind::Mic,
            name: "麦克风".into(),
            available: true,
            published: true,
        }
    }

    #[test]
    fn discovery_info_txt_roundtrip() {
        let info = DiscoveryInfo {
            v: DiscoveryInfo::VERSION,
            name: "卧室电脑".into(),
            roles: vec![RoleId::Relay, RoleId::Sender, RoleId::Viewer],
            media: vec![MediaKind::Screen, MediaKind::Mic],
            transports: vec![TransportId::Ws, TransportId::WebRtc],
            codecs: vec![CodecId::H264, CodecId::Aac],
            endpoints: vec![sample_summary()],
        };
        let txt = info.to_txt();
        // 多 key（§3.4 方案 b）：base + 每端点一个 key，均 ≤255B
        assert_eq!(txt.len(), 2, "base + 端点各占一个 key");
        assert_eq!(txt[0].0, TXT_KEY_DISCOVERY);
        assert!(txt[0].1.len() <= 255, "base TXT 应在 255B 内");
        assert!(
            txt[0]
                .1
                .contains("\"roles\":[\"relay\",\"sender\",\"viewer\"]"),
            "wire: {}",
            txt[0].1
        );
        assert!(!txt[0].1.contains("\"endpoints\""), "端点摘要移出 base key");
        assert_eq!(txt[1].0, "ep.0");
        assert!(txt[1].1.len() <= 255, "端点 TXT 应在 255B 内");
        assert!(txt[1].1.contains("\"endpointId\":\"mic:builtin\""));
        let back = DiscoveryInfo::from_txt(&txt).expect("roundtrip 解码");
        assert_eq!(info, back);
    }

    #[test]
    fn txt_multi_key_fits_255_per_platform_summary() {
        // 实测回归（§3.4/§11.1）：整包 449B 超限广播失败；多 key 后每 key 必须
        // ≤255B。用桌面三个端点的真实摘要验证（含不可用屏幕端点）。
        let endpoints = vec![
            EndpointSummary {
                endpoint_id: "screen:0".into(),
                kind: MediaKind::Screen,
                name: "屏幕".into(),
                available: false,
                published: true,
            },
            EndpointSummary {
                endpoint_id: "mic:builtin".into(),
                kind: MediaKind::Mic,
                name: "麦克风".into(),
                available: true,
                published: false,
            },
            EndpointSummary {
                endpoint_id: "sysaudio:builtin".into(),
                kind: MediaKind::SystemAudio,
                name: "系统声音".into(),
                available: true,
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
        .with_endpoints(endpoints);
        let txt = info.to_txt();
        for (k, v) in &txt {
            assert!(v.len() <= 255, "{k} 超 255B（{}B）: {v}", v.len());
        }
        let back = DiscoveryInfo::from_txt(&txt).expect("多 key roundtrip");
        assert_eq!(back.endpoints.len(), 3, "端点按序合并回");
        assert_eq!(back.endpoints[0].endpoint_id, "screen:0");
        assert_eq!(back.endpoints[1].kind, MediaKind::Mic);
        assert!(!back.endpoints[0].available);
        assert!(back.endpoints[0].published);
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
        // 无 endpoints 字段载荷 → 兼容解析，endpoints 默认空
        let v1 = r#"{"v":1,"name":"x","roles":["relay"],"media":[],"transports":[],"codecs":[]}"#;
        let back =
            DiscoveryInfo::from_txt(&[(TXT_KEY_DISCOVERY.into(), v1.into())]).expect("兼容解析");
        assert!(back.endpoints.is_empty());
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
