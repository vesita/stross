//! 端点注册表：节点上的 **设备表** + **已公开端点表**（一设备一端点 1:1）。
//!
//! 设计规格：docs/endpoint-model.md §6。
//!
//! * 设备：持久能力实体（`platform_devices` 静态枚举注入 + 文件端点动态设备）；
//! * 端点：设备被公开后的订阅入口（`publish` 实例化，`unpublish` 撤销）；
//! * P1 为 1:1（`endpoint_id == device_id`），重复公开报错；
//! * 文件端点：`publish_file` 登记本地文件源（路径不落 wire），订阅达成后
//!   由上层驱动（docs/endpoint-model.md §5 联动）推流；
//! * `on_subscribed` 携带 [`SubscribeCtx`]（定稿 delivery / 数据面流 id /
//!   push 模式的订阅方中继与凭证），上层据此开推。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use stross_proto::message::{
    CodecId, Delivery, DeviceInfo, DeviceSummary, EndpointManifest, EndpointState, MediaKind,
    TransportId, TransportPreference, Visibility,
};
use stross_proto::time::unix_secs;

use crate::error::{Error, Result};

/// 订阅事件载荷：上层驱动（文件泵 / 媒体推流）开推的依据。
///
/// 由协商层在**授予成功**后构造（docs/endpoint-model.md §5 联动）。
#[derive(Debug, Clone)]
pub struct SubscribeCtx {
    /// 订阅方节点 device_id。
    pub subscriber: String,
    /// 公开方定稿后的数据面方向。
    pub delivery: Delivery,
    /// 数据面流 id：pull = 公开方本机会话（内核预授权）；push = 订阅方自签会话。
    pub stream_id: String,
    /// push 模式：订阅方中继 HTTP 基址（`ws://ip:port`；公开方出站 push 目标）。
    pub relay_addr: Option<String>,
    /// push 模式：订阅方自签的一次性接入凭证（推流 Hello 出示）。
    pub share_token: Option<String>,
}

/// 文件端点本地文件源（路径只存本地，绝不进 wire / 目录 / 摘要）。
#[derive(Debug, Clone)]
pub struct FileSource {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
}

/// 订阅事件回调（上层注册；CLI serve 安装端点驱动，GUI 暂不接线）。
pub type SubscribeHook = dyn Fn(&str, &SubscribeCtx) + Send + Sync;

/// 端点注册表。
#[derive(Default)]
pub struct EndpointRegistry {
    devices: HashMap<String, DeviceInfo>,
    endpoints: HashMap<String, EndpointManifest>,
    /// 文件端点：endpoint_id → 本地文件源。
    file_sources: HashMap<String, FileSource>,
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

    /// 取消公开；文件端点顺带移除本地文件源（动态设备保留在设备表：
    /// 设备 = 持久能力实体，文档 §1）。
    pub fn unpublish(&mut self, endpoint_id: &str) -> Result<()> {
        if self.endpoints.remove(endpoint_id).is_none() {
            return Err(Error::Message(format!("端点不存在: {endpoint_id}")));
        }
        self.file_sources.remove(endpoint_id);
        Ok(())
    }

    /// 公开一个本地文件为文件端点（动态设备 `file:<名>`，重名自动加序号）。
    ///
    /// 返回的清单里 `device.kind == File`；本地路径登记进 `file_sources`
    /// （绝不出现在摘录 / 目录 / wire）。
    pub fn publish_file(
        &mut self,
        path: &std::path::Path,
        visibility: Visibility,
        delivery: Delivery,
    ) -> Result<EndpointManifest> {
        let meta = std::fs::metadata(path)
            .map_err(|e| Error::Message(format!("文件不可读 {}: {e}", path.display())))?;
        if !meta.is_file() {
            return Err(Error::Message(format!("不是普通文件: {}", path.display())));
        }
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "未命名".into());
        // 动态设备 id 唯一：首用 `file:<名>`，冲突加 `-2` / `-3` …
        let mut device_id = format!("file:{name}");
        let mut n = 2;
        while self.devices.contains_key(&device_id) {
            device_id = format!("file:{name}-{n}");
            n += 1;
        }
        self.devices.insert(
            device_id.clone(),
            DeviceInfo {
                device_id: device_id.clone(),
                kind: MediaKind::File,
                name: name.clone(),
                builtin: false, // 动态设备（随公开产生，非平台常驻）
            },
        );
        let size = meta.len();
        let manifest = self.publish(
            &device_id,
            visibility,
            delivery,
            Self::default_transports(MediaKind::File),
            vec![], // 文件无编解码
        )?;
        self.file_sources.insert(
            device_id.clone(),
            FileSource {
                path: path.to_path_buf(),
                name,
                size,
            },
        );
        Ok(manifest)
    }

    /// 文件端点的本地文件源（驱动开推用；非文件端点返回 `None`）。
    pub fn file_source(&self, endpoint_id: &str) -> Option<&FileSource> {
        self.file_sources.get(endpoint_id)
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

    /// 接线订阅事件回调（通常由上层启动时安装一次）。
    pub fn set_subscribe_hook(&mut self, hook: Option<Arc<SubscribeHook>>) {
        self.on_subscribed = hook;
    }

    /// 取订阅回调（克隆出锁再调用——hook 内部会再查注册表，持锁调用会死锁）。
    pub fn subscribed_hook(&self) -> Option<Arc<SubscribeHook>> {
        self.on_subscribed.clone()
    }

    /// 订阅达成事件（协商层授予成功后触发；携带开推所需上下文）。
    ///
    /// 注意：调用方切勿持有本注册表锁（hook 内部会再次访问注册表）。
    pub fn on_subscribed(&self, endpoint_id: &str, ctx: &SubscribeCtx) {
        if let Some(hook) = &self.on_subscribed {
            hook(endpoint_id, ctx);
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
        let ctx = SubscribeCtx {
            subscriber: "dev-phone".into(),
            delivery: Delivery::Push,
            stream_id: "sess-1".into(),
            relay_addr: Some("ws://192.168.1.5:9000".into()),
            share_token: Some("tok".into()),
        };
        r.set_subscribe_hook(Some(Arc::new(move |eid, c| {
            assert_eq!(eid, "mic:builtin");
            assert_eq!(c.subscriber, "dev-phone");
            assert_eq!(c.relay_addr.as_deref(), Some("ws://192.168.1.5:9000"));
            f.fetch_add(1, Ordering::SeqCst);
        })));
        r.on_subscribed("mic:builtin", &ctx);
        assert_eq!(fired.load(Ordering::SeqCst), 1);
        // 未接线时不触发也不 panic
        r.set_subscribe_hook(None);
        r.on_subscribed("mic:builtin", &ctx);
    }

    #[test]
    fn publish_file_registers_source_and_unpublish_clears() {
        let dir = std::env::temp_dir().join(format!("stross-reg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("备注.txt");
        std::fs::write(&path, b"hello stross").unwrap();
        let mut r = EndpointRegistry::new();
        let m = r
            .publish_file(&path, Visibility::Public, Delivery::Pull)
            .expect("公开文件端点");
        assert_eq!(m.device.kind, MediaKind::File);
        assert!(
            m.endpoint_id.starts_with("file:备注.txt"),
            "{}",
            m.endpoint_id
        );
        assert!(!m.device.builtin, "文件设备是动态设备");
        assert_eq!(m.endpoint_id, m.device.device_id, "P1 1:1");
        assert_eq!(m.transports.len(), 2, "文件 Lossless 默认 QUIC>WS");
        assert_eq!(m.transports[0].transport, TransportId::Quic);
        // 文件源可查（本地路径不落 wire：清单里没有 path 字段）
        let src = r.file_source(&m.endpoint_id).expect("文件源已登记");
        assert_eq!(src.name, "备注.txt");
        assert_eq!(src.size, b"hello stross".len() as u64);
        // 重名自动加序号
        let m2 = r
            .publish_file(&path, Visibility::Public, Delivery::Pull)
            .unwrap();
        assert_ne!(m.endpoint_id, m2.endpoint_id);
        // 摘要含动态设备
        assert!(r.summaries().iter().any(|d| d.kind == MediaKind::File));
        // 取消公开 → 文件源移除、端点消失（动态设备保留）
        r.unpublish(&m.endpoint_id).unwrap();
        assert!(r.file_source(&m.endpoint_id).is_none());
        assert!(r.manifest(&m.endpoint_id).is_none());
        assert!(r.unpublish(&m2.endpoint_id).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
