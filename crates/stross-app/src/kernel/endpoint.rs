//! 端点注册表（P1）：节点上的 **设备表** + **已公开端点表**（一设备一端点 1:1）。
//!
//! 设计规格：docs/endpoint-model.md §6。
//!
//! * 设备：持久能力实体（`platform_devices` 静态枚举注入）；
//! * 端点：设备被公开后的订阅入口（`publish` 实例化，`unpublish` 撤销）；
//! * P1 为 1:1（`endpoint_id == device_id`），重复公开报错；
//! * `on_subscribed` 只挂不接（P1）：后续步骤接线"自动建会话 + 推流"联动。

use std::collections::HashMap;
use std::sync::Arc;

use stross_proto::message::{
    CodecId, Delivery, DeviceInfo, DeviceSummary, EndpointManifest, EndpointState, MediaKind,
    TransportId, TransportPreference, Visibility,
};
use stross_proto::time::unix_secs;

use crate::error::{Error, Result};

/// 订阅事件回调（P1 只挂不接；后续接"自动建会话+推流"联动，pull 模式）。
pub type SubscribeHook = dyn Fn(&str) + Send + Sync;

/// 端点注册表。
#[derive(Default)]
pub struct EndpointRegistry {
    devices: HashMap<String, DeviceInfo>,
    endpoints: HashMap<String, EndpointManifest>,
    /// 订阅达成回调。
    on_subscribed: Option<Arc<SubscribeHook>>,
}

impl EndpointRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 以初始设备清单构造（P1：`app::platform_devices` 静态枚举）。
    pub fn with_devices(devices: Vec<DeviceInfo>) -> Self {
        let mut r = Self::new();
        for d in devices {
            r.register_device(d);
        }
        r
    }

    /// 注册设备；id 已存在时返回 `false`（不覆盖）。
    pub fn register_device(&mut self, device: DeviceInfo) -> bool {
        if self.devices.contains_key(&device.device_id) {
            return false;
        }
        self.devices.insert(device.device_id.clone(), device);
        true
    }

    /// 全部设备。
    pub fn devices(&self) -> Vec<DeviceInfo> {
        self.devices.values().cloned().collect()
    }

    /// 单个设备。
    pub fn device(&self, device_id: &str) -> Option<&DeviceInfo> {
        self.devices.get(device_id)
    }

    /// mDNS 摘要（L1）：设备 + 是否已公开（1:1 下判断 `endpoints` 是否含该 id）。
    pub fn summaries(&self) -> Vec<DeviceSummary> {
        self.devices
            .values()
            .map(|d| DeviceSummary::from_device(d, self.endpoints.contains_key(&d.device_id)))
            .collect()
    }

    /// 公开设备为端点（P1 1:1：同一设备重复公开报错；未知设备报错）。
    pub fn publish(
        &mut self,
        device_id: &str,
        visibility: Visibility,
        delivery: Delivery,
        transports: Vec<TransportPreference>,
        codecs: Vec<CodecId>,
    ) -> Result<EndpointManifest> {
        let device = self
            .devices
            .get(device_id)
            .cloned()
            .ok_or_else(|| Error::Message(format!("设备不存在: {device_id}")))?;
        if self.endpoints.contains_key(device_id) {
            return Err(Error::Message(format!("设备已公开: {device_id}")));
        }
        let manifest = EndpointManifest {
            endpoint_id: device_id.to_string(), // 1:1
            device,
            visibility,
            delivery,
            transports,
            codecs,
            state: EndpointState::Idle,
            subscribers: 0,
            updated_at: unix_secs(),
        };
        self.endpoints
            .insert(device_id.to_string(), manifest.clone());
        Ok(manifest)
    }

    /// 取消公开；已订阅会话由上层决定宽限期（P1 直接移除）。
    pub fn unpublish(&mut self, endpoint_id: &str) -> Result<()> {
        if self.endpoints.remove(endpoint_id).is_none() {
            return Err(Error::Message(format!("端点不存在: {endpoint_id}")));
        }
        Ok(())
    }

    /// 端点清单（订阅握手 / 目录 API 用）。
    pub fn manifest(&self, endpoint_id: &str) -> Option<&EndpointManifest> {
        self.endpoints.get(endpoint_id)
    }

    /// 全部端点清单。
    pub fn manifests(&self) -> Vec<EndpointManifest> {
        self.endpoints.values().cloned().collect()
    }

    /// 更新端点运行状态（Idle/Active/Suspended + 订阅数）。
    pub fn set_state(&mut self, endpoint_id: &str, state: EndpointState, subscribers: u32) -> bool {
        let Some(m) = self.endpoints.get_mut(endpoint_id) else {
            return false;
        };
        m.state = state;
        m.subscribers = subscribers;
        m.updated_at = unix_secs();
        true
    }

    /// 接线订阅事件回调（P1 只挂不接，见模块注释）。
    pub fn set_subscribe_hook(&mut self, hook: Option<Arc<SubscribeHook>>) {
        self.on_subscribed = hook;
    }

    /// 订阅达成事件（pull 模式：公开方收到订阅 → 触发上层"建会话 + 推流"）。
    pub fn on_subscribed(&self, endpoint_id: &str) {
        if let Some(hook) = &self.on_subscribed {
            hook(endpoint_id);
        }
    }

    /// 端点类型默认传输（按 ReliabilityProfile 契约，公开者选协议的缺省）：
    /// 媒体类（Lossy/Adaptive）→ QUIC > SRT > WS；其余（Lossless）→ QUIC > WS。
    pub fn default_transports(kind: MediaKind) -> Vec<TransportPreference> {
        use MediaKind::{Camera, Mic, Screen, SystemAudio};
        let is_lossy = matches!(kind, Screen | Camera | Mic | SystemAudio);
        if is_lossy {
            vec![
                TransportPreference {
                    transport: TransportId::Quic,
                    priority: 0,
                },
                TransportPreference {
                    transport: TransportId::Srt,
                    priority: 1,
                },
                TransportPreference {
                    transport: TransportId::Ws,
                    priority: 2,
                },
            ]
        } else {
            vec![
                TransportPreference {
                    transport: TransportId::Quic,
                    priority: 0,
                },
                TransportPreference {
                    transport: TransportId::Ws,
                    priority: 1,
                },
            ]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn mic() -> DeviceInfo {
        DeviceInfo {
            device_id: "mic:builtin".into(),
            kind: MediaKind::Mic,
            name: "麦克风".into(),
            builtin: true,
        }
    }

    #[test]
    fn publish_one_to_one_state_and_unpublish() {
        let mut r = EndpointRegistry::with_devices(vec![mic()]);
        assert_eq!(r.devices().len(), 1);

        // 公开（1:1：endpoint_id == device_id）
        let m = r
            .publish(
                "mic:builtin",
                Visibility::Public,
                Delivery::Pull,
                EndpointRegistry::default_transports(MediaKind::Mic),
                vec![CodecId::Aac],
            )
            .unwrap();
        assert_eq!(m.endpoint_id, "mic:builtin");
        assert_eq!(m.state, EndpointState::Idle);
        assert_eq!(m.subscribers, 0);

        // 1:1：重复公开报错
        assert!(
            r.publish(
                "mic:builtin",
                Visibility::Public,
                Delivery::Pull,
                vec![],
                vec![]
            )
            .is_err()
        );
        // 未知设备报错
        assert!(
            r.publish("nope", Visibility::Public, Delivery::Pull, vec![], vec![])
                .is_err()
        );

        // 状态与订阅数
        assert!(r.set_state("mic:builtin", EndpointState::Active, 2));
        let m = r.manifest("mic:builtin").unwrap();
        assert_eq!(m.state, EndpointState::Active);
        assert_eq!(m.subscribers, 2);
        // 未知端点 set_state 返回 false
        assert!(!r.set_state("nope", EndpointState::Active, 0));

        // 摘要携带 published（1:1 判断）
        let s = r.summaries();
        assert!(s.iter().any(|d| d.published));

        // 取消公开
        assert!(r.unpublish("mic:builtin").is_ok());
        assert!(r.unpublish("mic:builtin").is_err());
        assert!(r.manifest("mic:builtin").is_none());
        // 取消后摘要 published 归 false
        assert!(r.summaries().iter().all(|d| !d.published));
    }

    #[test]
    fn register_device_dedup() {
        let mut r = EndpointRegistry::new();
        assert!(r.register_device(mic()));
        assert!(!r.register_device(mic()), "重复注册不覆盖");
    }

    #[test]
    fn default_transports_by_kind() {
        let lossy = EndpointRegistry::default_transports(MediaKind::Mic);
        assert_eq!(
            lossy.iter().map(|t| t.transport).collect::<Vec<_>>(),
            vec![TransportId::Quic, TransportId::Srt, TransportId::Ws]
        );
        let lossless = EndpointRegistry::default_transports(MediaKind::Clipboard);
        assert_eq!(
            lossless.iter().map(|t| t.transport).collect::<Vec<_>>(),
            vec![TransportId::Quic, TransportId::Ws]
        );
    }

    #[test]
    fn subscribe_hook_fires_only_when_set() {
        let mut r = EndpointRegistry::with_devices(vec![mic()]);
        r.publish(
            "mic:builtin",
            Visibility::Confirm,
            Delivery::Push,
            vec![],
            vec![],
        )
        .unwrap();
        let fired = Arc::new(AtomicUsize::new(0));
        let f = fired.clone();
        r.set_subscribe_hook(Some(Arc::new(move |eid| {
            assert_eq!(eid, "mic:builtin");
            f.fetch_add(1, Ordering::SeqCst);
        })));
        r.on_subscribed("mic:builtin");
        assert_eq!(fired.load(Ordering::SeqCst), 1);
        // 未接线时不触发也不 panic
        r.set_subscribe_hook(None);
        r.on_subscribed("mic:builtin");
    }
}
