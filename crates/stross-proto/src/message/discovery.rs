//! mDNS 发现能力引导（F1.2：单 key JSON，新增字段零维护）。

use serde::{Deserialize, Serialize};

use super::endpoint::EndpointSummary;
use super::ids::{CodecId, MediaKind, RoleId, TransportId};

/// mDNS TXT 中承载 [`DiscoveryInfo`] 的固定 key。
pub const TXT_KEY_DISCOVERY: &str = "stross";

/// mDNS TXT 单条 character-string 的字节上限（RFC 1035 §3.3；mdns-sd 强校验）。
pub const TXT_MAX_BYTES: usize = 255;

/// [`DiscoveryInfo::to_txt`] 编码错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryError {
    /// 基础能力 key（`stross`）序列化后超 [`TXT_MAX_BYTES`]。
    BaseTooLarge(usize),
    /// 某个端点摘要 key（`ep.<n>`）序列化后超 [`TXT_MAX_BYTES`]。
    EndpointTooLarge(usize),
}

impl std::fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BaseTooLarge(len) => write!(
                f,
                "mDNS 基础能力 TXT 超 {TXT_MAX_BYTES}B（实际 {len}B）：设备名/角色/媒体过长"
            ),
            Self::EndpointTooLarge(len) => {
                write!(
                    f,
                    "mDNS 端点摘要 TXT 超 {TXT_MAX_BYTES}B（实际 {len}B）：端点名过长"
                )
            }
        }
    }
}

impl std::error::Error for DiscoveryError {}

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
    ///
    /// **返回 `Result` 而非此前静默 `debug_assert`**：release 构建下 mdns 层会
    /// 对超长单条 key 直接 `truncate(255)`，把半截 JSON 广播出去 → 浏览端解析
    /// 失败返回 `None`，整台设备静默消失且无日志。这里把超长判定上升到运行时
    /// 错误，由调用方在广播前降级（截断设备名 / 丢弃过长端点摘要）。
    pub fn to_txt(&self) -> Result<Vec<(String, String)>, DiscoveryError> {
        let mut out = Vec::with_capacity(1 + self.endpoints.len());
        let mut base = self.clone();
        base.endpoints = Vec::new();
        let base_json = serde_json::to_string(&base).expect("DiscoveryInfo 序列化不应失败");
        if base_json.len() > TXT_MAX_BYTES {
            return Err(DiscoveryError::BaseTooLarge(base_json.len()));
        }
        out.push((TXT_KEY_DISCOVERY.to_string(), base_json));
        for (i, e) in self.endpoints.iter().enumerate() {
            let ep_json = serde_json::to_string(e).expect("端点摘要序列化不应失败");
            if ep_json.len() > TXT_MAX_BYTES {
                return Err(DiscoveryError::EndpointTooLarge(ep_json.len()));
            }
            out.push((format!("ep.{i}"), ep_json));
        }
        Ok(out)
    }

    /// 容错编码：广播**尽力而为**——宁可降级局部，也不让整条广播失败。
    ///
    /// * 设备名过长 → 按字符边界截短（字节递减）直到 base 收进 [`TXT_MAX_BYTES`]；
    /// * 单个端点摘要过长 → 丢弃该端点（不阻断其它端点 / base 广播）。
    ///
    /// 返回 `(props, degraded)`：`degraded` 指示是否有内容被丢弃 / 截断，
    /// 便于调用方记录日志。mDNS 广播（stross-kernel discovery::mdns）用此方法；
    /// 想要严格失败语义的调用方用 [`DiscoveryInfo::to_txt`]。
    pub fn to_txt_lenient(&self) -> (Vec<(String, String)>, bool) {
        let mut degraded = false;
        let mut base = self.clone();
        base.endpoints = Vec::new();
        let mut base_json = serde_json::to_string(&base).expect("DiscoveryInfo 序列化不应失败");
        // 设备名过长：逐个**整字符**（`String::pop`）移除尾部字符，直到 base 收进
        // 上限——按字节递减会切断多字节 UTF-8 走非字符边界 → truncate panic。
        while base_json.len() > TXT_MAX_BYTES && base.name.pop().is_some() {
            base_json = serde_json::to_string(&base).expect("DiscoveryInfo 序列化不应失败");
            degraded = true;
        }
        let mut out = Vec::with_capacity(1 + self.endpoints.len());
        out.push((TXT_KEY_DISCOVERY.to_string(), base_json));
        for (i, e) in self.endpoints.iter().enumerate() {
            let ep_json = serde_json::to_string(e).expect("端点摘要序列化不应失败");
            if ep_json.len() > TXT_MAX_BYTES {
                degraded = true;
                continue; // 丢弃过长端点摘要，不阻断广播
            }
            out.push((format!("ep.{i}"), ep_json));
        }
        (out, degraded)
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
            endpoint_id: 0,
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
        let txt = info.to_txt().expect("正常摘要应编码成功");
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
        assert!(txt[1].1.contains("\"endpointId\":0"));
        let back = DiscoveryInfo::from_txt(&txt).expect("roundtrip 解码");
        assert_eq!(info, back);
    }

    #[test]
    fn txt_multi_key_fits_255_per_platform_summary() {
        // 实测回归（§3.4/§11.1）：整包 449B 超限广播失败；多 key 后每 key 必须
        // ≤255B。用桌面三个端点的真实摘要验证（含不可用屏幕端点）。
        let endpoints = vec![
            EndpointSummary {
                endpoint_id: 0,
                kind: MediaKind::Screen,
                name: "屏幕".into(),
                available: false,
                published: true,
            },
            EndpointSummary {
                endpoint_id: 0,
                kind: MediaKind::Mic,
                name: "麦克风".into(),
                available: true,
                published: false,
            },
            EndpointSummary {
                endpoint_id: 0,
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
        let txt = info.to_txt().expect("桌面摘要应编码成功");
        for (k, v) in &txt {
            assert!(v.len() <= 255, "{k} 超 255B（{}B）: {v}", v.len());
        }
        let back = DiscoveryInfo::from_txt(&txt).expect("多 key roundtrip");
        assert_eq!(back.endpoints.len(), 3, "端点按序合并回");
        assert_eq!(back.endpoints[0].endpoint_id, 0);
        assert_eq!(back.endpoints[1].kind, MediaKind::Mic);
        assert!(!back.endpoints[0].available);
        assert!(back.endpoints[0].published);
    }

    #[test]
    fn to_txt_rejects_oversized_instead_of_silent_truncate() {
        // 回归：release 下 mdns 层会对超长单条 key 直接 truncate(255)，
        // 导致半截 JSON 广播、对端解析失败、设备静默消失。to_txt 必须返回
        // 运行时错误而非 debug_assert 吞掉。
        //
        // 超长设备名 → BaseTooLarge
        let long_name = "超长设备名".repeat(60); // 60×6+3 = 363+ 字节，远超 255
        let base = DiscoveryInfo::relay_default(long_name, vec![MediaKind::Screen, MediaKind::Mic]);
        match base.to_txt() {
            Err(DiscoveryError::BaseTooLarge(len)) => assert!(len > 255, "应报告实际长度 {len}"),
            other => panic!("超长设备名应报 BaseTooLarge，得到 {other:?}"),
        }

        // 超长端点名 → EndpointTooLarge（base 用短名，端点用长名）
        let base = DiscoveryInfo::relay_default("正常设备".to_string(), vec![MediaKind::Mic]);
        let info = base.with_endpoints(vec![EndpointSummary {
            endpoint_id: 0,
            kind: MediaKind::Mic,
            name: "超长端点名".repeat(60),
            available: true,
            published: true,
        }]);
        match info.to_txt() {
            Err(DiscoveryError::EndpointTooLarge(len)) => assert!(len > 255),
            other => panic!("超长端点名应报 EndpointTooLarge，得到 {other:?}"),
        }
    }

    #[test]
    fn to_txt_lenient_degrades_instead_of_failing() {
        // 广播尽力而为：超长 device 名被截短、超长端点名被丢弃，但 base 仍正常产出。
        let long_name = "超长设备名".repeat(60);
        let info = DiscoveryInfo::relay_default(long_name, vec![MediaKind::Screen, MediaKind::Mic])
            .with_endpoints(vec![
                EndpointSummary {
                    endpoint_id: 0,
                    kind: MediaKind::Mic,
                    name: "超长端点名".repeat(60), // 超 255B → 应被丢弃
                    available: true,
                    published: true,
                },
                EndpointSummary {
                    endpoint_id: 1,
                    kind: MediaKind::Screen,
                    name: "屏幕".into(),
                    available: true,
                    published: true,
                },
            ]);

        let (props, degraded) = info.to_txt_lenient();
        assert!(degraded, "超长 name/端点应被标记降级");
        // base key 恒在，且每条 ≤255B
        let base_json = props
            .iter()
            .find(|(k, _)| k == TXT_KEY_DISCOVERY)
            .expect("base key 必须存在")
            .1
            .clone();
        assert!(
            base_json.len() <= TXT_MAX_BYTES,
            "base {len}B",
            len = base_json.len()
        );
        // 超长端点被丢弃，正常端点（ep.1）保留
        assert!(
            !props.iter().any(|(_, v)| v.contains("超长端点名")),
            "超长端点摘要应被丢弃"
        );
        let screen_ep = props
            .iter()
            .find(|(k, _)| k == "ep.1")
            .expect("正常端点 ep.1 应保留");
        assert!(
            screen_ep.1.contains("屏幕"),
            "正常端点摘要保留: {}",
            screen_ep.1
        );
        // 降级后仍可被浏览侧解析（不产生半截 JSON）
        let back = DiscoveryInfo::from_txt(&props).expect("降级产物应可被浏览侧解码");
        assert_eq!(back.endpoints.len(), 1, "只剩正常端点");
        assert_eq!(back.endpoints[0].name, "屏幕");
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
