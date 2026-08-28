//! 端点注册表：**单层端点表**（原「设备表 + 端点表」合并）+ 通告参数管理。
//!
//! 设计规格：docs/endpoint-model.md。
//!
//! * 端点 = 节点上可共享的能力实体；**契约（[`Endpoint`] / [`SubscribeCtx`] /
//!   [`Probe`] 等）与具体端点实现（屏幕 / 麦克风 / 系统声音 / 文件）在
//!   [`stross_endpoint`] 插件区**，本模块只做身份登记、通告参数管理与订阅联动
//!   （内核 = 纯管理调度，不做媒体数据面）；
//! * 端点自维护「可挂载性」（`available`，load 探测结果）与失败原因
//!   （`last_error`）；注册表只做身份登记与通告参数管理；
//! * **订阅联动**：`on_subscribed` 出锁克隆端点对象后调用其 `share`
//!   （端点自驱动，内核不做类型分派）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use stross_endpoint::contract::{Endpoint, SubscribeCtx, TargetKind};
use stross_endpoint::file::FileEndpoint;
use stross_proto::message::{
    CodecId, Delivery, EndpointManifest, EndpointState, EndpointSummary, TransportId,
    TransportPreference, Visibility,
};
use stross_proto::time::unix_secs;

use crate::Kernel;
use crate::error::{Error, Result};

/// 端点条目：行为对象（[`Endpoint`]）+ 通告参数（公开者声明）。
pub struct EndpointEntry {
    pub ep: Arc<dyn Endpoint>,
    pub published: bool,
    pub visibility: Visibility,
    pub delivery: Delivery,
    pub transports: Vec<TransportPreference>,
    pub codecs: Vec<CodecId>,
    pub state: EndpointState,
    pub subscribers: u32,
    pub updated_at: u64,
}

/// 端点注册表：**单层端点表**（原「设备表 + 端点表」合并）。
///
/// 端点自维护可挂载性（`load` 探测）；注册表只做身份登记与通告参数管理。
#[derive(Default)]
pub struct EndpointRegistry {
    endpoints: HashMap<String, EndpointEntry>,
    /// 文件端点：endpoint_id → 本地文件源（control.rs 状态展示）。
    file_sources: HashMap<String, FileSource>,
}

impl EndpointRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记端点并立即 `load`（探测可挂载性）；id 已存在时返回 `false`。
    ///
    /// load 失败不阻止登记：端点保留在表里但标记不可挂载（`available=false`
    /// + `last_error`）——UI 可见原因，不可通告/订阅。
    pub fn seed(&mut self, mut ep: Box<dyn Endpoint>) -> bool {
        let id = ep.id().to_string();
        if self.endpoints.contains_key(&id) {
            return false;
        }
        if let Err(e) = ep.load() {
            tracing::warn!("端点 {id} load 失败，标记不可挂载: {e}");
        }
        self.endpoints.insert(
            id,
            EndpointEntry {
                ep: Arc::from(ep),
                published: false,
                visibility: Visibility::Public,
                delivery: Delivery::Pull,
                transports: vec![],
                codecs: vec![],
                state: EndpointState::Idle,
                subscribers: 0,
                updated_at: unix_secs(),
            },
        );
        true
    }

    /// 端点行为对象（`on_subscribed` 出锁调用用；持锁调用会死锁）。
    pub fn endpoint_arc(&self, endpoint_id: &str) -> Option<Arc<dyn Endpoint>> {
        self.endpoints.get(endpoint_id).map(|e| e.ep.clone())
    }

    /// 端点目标类型（缺省传输选择用）。
    pub fn target(&self, endpoint_id: &str) -> Option<TargetKind> {
        self.endpoints.get(endpoint_id).map(|e| e.ep.target())
    }

    /// 全部端点清单（本机目录用；含未通告）。
    pub fn manifests(&self) -> Vec<EndpointManifest> {
        self.endpoints.values().map(Self::manifest_of).collect()
    }

    /// 已通告端点清单（对端目录用；Private 过滤由调用方做）。
    pub fn published_manifests(&self) -> Vec<EndpointManifest> {
        self.endpoints
            .values()
            .filter(|e| e.published)
            .map(Self::manifest_of)
            .collect()
    }

    /// mDNS 摘要（L1）：全部端点（含不可挂载 + 未通告标记）。
    pub fn summaries(&self) -> Vec<EndpointSummary> {
        self.endpoints
            .values()
            .map(|e| EndpointSummary {
                endpoint_id: e.ep.id().to_string(),
                kind: e.ep.kind(),
                name: e.ep.name().to_string(),
                available: e.ep.available(),
                published: e.published,
            })
            .collect()
    }

    fn manifest_of(entry: &EndpointEntry) -> EndpointManifest {
        EndpointManifest {
            endpoint_id: entry.ep.id().to_string(),
            kind: entry.ep.kind(),
            name: entry.ep.name().to_string(),
            available: entry.ep.available(),
            last_error: entry.ep.last_error().map(str::to_string),
            published: entry.published,
            visibility: entry.visibility.clone(),
            delivery: entry.delivery,
            transports: entry.transports.clone(),
            codecs: entry.codecs.clone(),
            state: entry.state,
            subscribers: entry.subscribers,
            updated_at: entry.updated_at,
        }
    }

    /// 端点清单（订阅握手 / 目录 API 用）。
    pub fn manifest(&self, endpoint_id: &str) -> Option<EndpointManifest> {
        self.endpoints.get(endpoint_id).map(Self::manifest_of)
    }

    /// 通告端点（设置可见性 / delivery / 传输；不可挂载端点拒绝）。
    pub fn publish(
        &mut self,
        endpoint_id: &str,
        visibility: Visibility,
        delivery: Delivery,
        transports: Vec<TransportPreference>,
        codecs: Vec<CodecId>,
    ) -> Result<EndpointManifest> {
        let entry = self
            .endpoints
            .get_mut(endpoint_id)
            .ok_or_else(|| Error::Message(format!("端点不存在: {endpoint_id}")))?;
        if !entry.ep.available() {
            let reason = entry.ep.last_error().unwrap_or("未知原因").to_string();
            return Err(Error::Message(format!(
                "端点不可挂载（{reason}）: {endpoint_id}"
            )));
        }
        if entry.published {
            return Err(Error::Message(format!("端点已通告: {endpoint_id}")));
        }
        entry.published = true;
        entry.visibility = visibility;
        entry.delivery = delivery;
        entry.transports = transports;
        entry.codecs = codecs;
        entry.updated_at = unix_secs();
        Ok(Self::manifest_of(entry))
    }

    /// 取消通告（端点保留在表里：可再次通告；文件端点顺带移除文件源登记）。
    pub fn unpublish(&mut self, endpoint_id: &str) -> Result<()> {
        let entry = self
            .endpoints
            .get_mut(endpoint_id)
            .ok_or_else(|| Error::Message(format!("端点不存在: {endpoint_id}")))?;
        if !entry.published {
            return Err(Error::Message(format!("端点未通告: {endpoint_id}")));
        }
        entry.published = false;
        entry.visibility = Visibility::Public;
        entry.delivery = Delivery::Pull;
        entry.transports = vec![];
        entry.codecs = vec![];
        entry.state = EndpointState::Idle;
        entry.subscribers = 0;
        entry.updated_at = unix_secs();
        self.file_sources.remove(endpoint_id);
        Ok(())
    }

    /// 公开一个本地文件为文件端点（确定目标，动态端点 `file:<名>`，重名加序号）。
    ///
    /// 返回的清单里 `kind == File`；本地路径登记进端点对象与 `file_sources`
    /// （绝不出现在摘录 / 目录 / wire）。
    pub fn publish_file(
        &mut self,
        path: &Path,
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
        let mut endpoint_id = format!("file:{name}");
        let mut n = 2;
        while self.endpoints.contains_key(&endpoint_id) {
            endpoint_id = format!("file:{name}-{n}");
            n += 1;
        }
        let size = meta.len();
        let ep = FileEndpoint::new(endpoint_id.clone(), name.clone(), path.to_path_buf());
        if !self.seed(Box::new(ep)) {
            return Err(Error::Message(format!("端点已存在: {endpoint_id}")));
        }
        self.file_sources.insert(
            endpoint_id.clone(),
            FileSource {
                path: path.to_path_buf(),
                name: name.clone(),
                size,
            },
        );
        self.publish(
            &endpoint_id,
            visibility,
            delivery,
            Self::default_transports(TargetKind::Determined),
            vec![], // 文件无编解码
        )
    }

    /// 文件端点的本地文件源（control.rs 状态展示；非文件端点返回 `None`）。
    pub fn file_source(&self, endpoint_id: &str) -> Option<&FileSource> {
        self.file_sources.get(endpoint_id)
    }

    /// 更新端点运行状态（Idle/Active/Suspended + 订阅数）。
    pub fn set_state(&mut self, endpoint_id: &str, state: EndpointState, subscribers: u32) -> bool {
        let Some(entry) = self.endpoints.get_mut(endpoint_id) else {
            return false;
        };
        entry.state = state;
        entry.subscribers = subscribers;
        entry.updated_at = unix_secs();
        true
    }

    /// 订阅达成事件：出锁克隆端点对象后调用其 `share`（端点自驱动，
    /// 内核不做类型分派）。注意：调用方切勿持有本注册表锁。
    pub fn on_subscribed(&self, app: &Arc<Kernel>, endpoint_id: &str, ctx: &SubscribeCtx) {
        let Some(ep) = self.endpoint_arc(endpoint_id) else {
            return;
        };
        ep.share(app.clone(), ctx.clone());
    }

    /// 端点默认传输（按目标类型，ReliabilityProfile 契约）：
    /// 实时目标（Lossy/Adaptive）→ QUIC > SRT > WS；确定目标（Lossless）→
    /// QUIC > WS。**不再按 MediaKind 枚举匹配**——新增端点类型按目标类型
    /// 自动获得正确策略。
    pub fn default_transports(target: TargetKind) -> Vec<TransportPreference> {
        let p = |transport: TransportId, priority: u8| TransportPreference {
            transport,
            priority,
        };
        match target {
            TargetKind::Live => vec![
                p(TransportId::Quic, 0),
                p(TransportId::Srt, 1),
                p(TransportId::Ws, 2),
            ],
            TargetKind::Determined => vec![p(TransportId::Quic, 0), p(TransportId::Ws, 1)],
        }
    }
}

/// 文件端点本地文件源（`control.rs` 状态展示用；路径不落 wire）。
#[derive(Debug, Clone)]
pub struct FileSource {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::result::Result as StdResult;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use stross_endpoint::contract::Probe;
    use stross_proto::message::MediaKind;

    fn ok_probe() -> Probe {
        Arc::new(|| Ok(()))
    }

    fn fail_probe(reason: &'static str) -> Probe {
        let r = reason.to_string();
        Arc::new(move || Err(r.clone()))
    }

    fn screen() -> Box<dyn Endpoint> {
        Box::new(stross_endpoint::screen::ScreenEndpoint::new(
            "屏幕",
            ok_probe(),
        ))
    }

    #[test]
    fn seed_loads_and_marks_availability() {
        let mut r = EndpointRegistry::new();
        // 可用端点：load 成功 → available
        assert!(r.seed(screen()));
        let m = r.manifest("screen:0").unwrap();
        assert!(m.available);
        assert!(m.last_error.is_none());
        assert!(!m.published, "登记后未通告");
        // 不可用端点（探测失败）：保留在表里但标记不可挂载 + 原因
        let mut r2 = EndpointRegistry::new();
        assert!(
            r2.seed(Box::new(stross_endpoint::screen::ScreenEndpoint::new(
                "屏幕",
                fail_probe("无图形会话（DISPLAY / WAYLAND_DISPLAY 均未设置）")
            )))
        );
        let m2 = r2.manifest("screen:0").unwrap();
        assert!(!m2.available);
        assert_eq!(
            m2.last_error.as_deref(),
            Some("无图形会话（DISPLAY / WAYLAND_DISPLAY 均未设置）")
        );
        // 不可挂载端点拒绝通告（错误携带原因）
        assert!(
            r2.publish(
                "screen:0",
                Visibility::Public,
                Delivery::Pull,
                vec![],
                vec![]
            )
            .is_err()
        );
    }

    #[test]
    fn publish_one_to_one_state_and_unpublish() {
        let mut r = EndpointRegistry::new();
        assert!(r.seed(screen()));
        assert!(!r.seed(screen()), "重复登记不覆盖");

        let m = r
            .publish(
                "screen:0",
                Visibility::Public,
                Delivery::Pull,
                EndpointRegistry::default_transports(TargetKind::Live),
                vec![CodecId::H264],
            )
            .unwrap();
        assert_eq!(m.endpoint_id, "screen:0");
        assert!(m.published);
        assert_eq!(m.state, EndpointState::Idle);
        assert_eq!(m.subscribers, 0);
        assert_eq!(
            m.transports[0].transport,
            TransportId::Quic,
            "实时目标默认 QUIC 优先"
        );

        // 重复通告报错
        assert!(
            r.publish(
                "screen:0",
                Visibility::Public,
                Delivery::Pull,
                vec![],
                vec![]
            )
            .is_err()
        );
        // 未知端点报错
        assert!(
            r.publish("nope", Visibility::Public, Delivery::Pull, vec![], vec![])
                .is_err()
        );

        // 状态与订阅数
        assert!(r.set_state("screen:0", EndpointState::Active, 2));
        let m = r.manifest("screen:0").unwrap();
        assert_eq!(m.state, EndpointState::Active);
        assert_eq!(m.subscribers, 2);
        assert!(!r.set_state("nope", EndpointState::Active, 0));

        // 摘要携带 available + published
        let s = r.summaries();
        assert!(s.iter().any(|e| e.published && e.available));

        // 取消通告（端点保留，可再次通告）
        assert!(r.unpublish("screen:0").is_ok());
        assert!(r.unpublish("screen:0").is_err());
        assert!(!r.manifest("screen:0").unwrap().published);
        assert!(
            r.publish(
                "screen:0",
                Visibility::Public,
                Delivery::Pull,
                vec![],
                vec![]
            )
            .is_ok()
        );
    }

    #[test]
    fn default_transports_by_target() {
        let live = EndpointRegistry::default_transports(TargetKind::Live);
        assert_eq!(
            live.iter().map(|t| t.transport).collect::<Vec<_>>(),
            vec![TransportId::Quic, TransportId::Srt, TransportId::Ws]
        );
        let determined = EndpointRegistry::default_transports(TargetKind::Determined);
        assert_eq!(
            determined.iter().map(|t| t.transport).collect::<Vec<_>>(),
            vec![TransportId::Quic, TransportId::Ws]
        );
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
        assert_eq!(m.kind, MediaKind::File);
        assert!(
            m.endpoint_id.starts_with("file:备注.txt"),
            "{}",
            m.endpoint_id
        );
        assert!(m.available, "文件端点 load 应探测可读");
        assert_eq!(m.transports.len(), 2, "确定目标默认 QUIC>WS");
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
        // 摘要含动态端点
        assert!(r.summaries().iter().any(|e| e.kind == MediaKind::File));
        // 取消通告 → 文件源移除、published 归 false（端点保留）
        r.unpublish(&m.endpoint_id).unwrap();
        assert!(r.file_source(&m.endpoint_id).is_none());
        assert!(!r.manifest(&m.endpoint_id).unwrap().published);
        assert!(r.unpublish(&m2.endpoint_id).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn on_subscribed_calls_endpoint_share_outside_lock() {
        // 端点自驱动：订阅事件 → share 被调用（内核不分派）
        let fired = Arc::new(AtomicUsize::new(0));
        let f = fired.clone();
        struct CountingEndpoint {
            base: stross_endpoint::contract::EndpointBase,
            fired: Arc<AtomicUsize>,
        }
        impl Endpoint for CountingEndpoint {
            fn id(&self) -> &str {
                &self.base.id
            }
            fn kind(&self) -> MediaKind {
                self.base.kind
            }
            fn name(&self) -> &str {
                &self.base.name
            }
            fn target(&self) -> TargetKind {
                TargetKind::Live
            }
            fn available(&self) -> bool {
                self.base.available
            }
            fn last_error(&self) -> Option<&str> {
                self.base.last_error.as_deref()
            }
            fn load(&mut self) -> StdResult<(), String> {
                self.base.available = true;
                Ok(())
            }
            fn share(
                &self,
                _app: Arc<dyn stross_endpoint::contract::EndpointApp>,
                ctx: SubscribeCtx,
            ) {
                assert_eq!(ctx.subscriber, "dev-phone");
                self.fired.fetch_add(1, Ordering::SeqCst);
            }
        }
        let mut r = EndpointRegistry::new();
        r.seed(Box::new(CountingEndpoint {
            base: stross_endpoint::contract::EndpointBase {
                id: "rec:0".into(),
                kind: MediaKind::Mic,
                name: "录音".into(),
                available: false,
                last_error: None,
            },
            fired: f.clone(),
        }));
        r.publish("rec:0", Visibility::Confirm, Delivery::Push, vec![], vec![])
            .unwrap();
        let ctx = SubscribeCtx {
            subscriber: "dev-phone".into(),
            delivery: Delivery::Push,
            stream_id: "sess-1".into(),
            relay_addr: Some("ws://192.168.1.5:9000".into()),
            share_token: Some("tok".into()),
        };
        let app = Arc::new(Kernel::new(crate::Platform::Desktop));
        r.on_subscribed(&app, "rec:0", &ctx);
        assert_eq!(fired.load(Ordering::SeqCst), 1);
        // 未知端点不触发
        r.on_subscribed(&app, "nope", &ctx);
        assert_eq!(fired.load(Ordering::SeqCst), 1);
    }
}
