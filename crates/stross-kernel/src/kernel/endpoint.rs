//! 端点框架：**单层端点模型** + 端点 ↔ 内核行为契约。
//!
//! 设计规格：docs/endpoint-model.md。
//!
//! * **端点** = 节点上可共享的能力实体（屏幕 / 麦克风 / 摄像头 / 系统声音 /
//!   文件……）：自维护「可挂载性」（`available`，load 探测结果）、失败原因
//!   （`last_error`）与通告状态（`published`）；
//! * **行为契约**（与内核的约定，非语言特性）：每个端点实现两个约定行为——
//!   `load`（探测自身可用性，能否被挂载成节点）与 `share`（订阅达成后启动
//!   共享推流，类型自决）——内核不做类型分派；
//! * **目标类型**（[`TargetKind`]）：端点分两类——确定目标（文件等，内容
//!   预先确定，一次推送、有完成态、Lossless）与实时目标（相机等，内容持续
//!   产生，持续推流、Lossy）；两类的共性即本文件的契约，差异经目标类型
//!   维度 + 各端点实现表达。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use stross_media::pipeline::{AudioSourceConfig, Quality, StreamConfig, VideoSource};
use stross_proto::message::{
    CodecId, Delivery, EndpointManifest, EndpointState, EndpointSummary, MediaKind, TransportId,
    TransportPreference, Visibility,
};
use stross_proto::time::unix_secs;

use crate::Kernel;
use crate::error::{Error, Result};
use crate::file_xfer::{FilePushOptions, push_file};
use std::result::Result as StdResult;

/// 目标类型：端点分两类的维度（docs/endpoint-model.md §1）。
///
/// 决定默认传输（Lossless / Lossy）与共享生命周期（一次推送 / 持续推流）；
/// **不进 wire**（`MediaKind` 已足够标识，避免冗余字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    /// 确定目标：内容在共享前已定（文件 / 剪贴板），一次推送、有完成态。
    Determined,
    /// 实时目标：内容持续产生（屏幕 / 相机 / 麦克风 / 系统声音），持续推流。
    Live,
}

/// load 探测函数：平台适应层注入（查环境 / 设备 / 权限），内核零 OS 调用。
pub type Probe = Arc<dyn Fn() -> StdResult<(), String> + Send + Sync>;

/// 端点公共身份 + 挂载状态（各具体端点的共有字段）。
#[derive(Debug)]
pub struct EndpointBase {
    pub id: String,
    pub kind: MediaKind,
    pub name: String,
    /// 能否被挂载成节点（load 探测结果；false = 不可通告、不可订阅）。
    pub available: bool,
    /// load/share 失败原因（UI / 目录展示）。
    pub last_error: Option<String>,
}

impl EndpointBase {
    /// 标记不可挂载并记录原因（load / share 失败时）。
    fn mark_failed(&mut self, e: String) {
        self.available = false;
        self.last_error = Some(e);
    }
}

/// 端点 ↔ 内核行为契约。
///
/// 每个端点必须实现两个约定行为（不是语言特性，是与内核的约定）：
/// * `load`：探测自身可用性（能否被挂载成节点），维护 `available` / `last_error`；
/// * `share`：订阅达成后启动共享（推流），类型自决，内核不做分派。
///
/// 端点自维护「可挂载性」：`available() == false` 时不可通告、不可订阅。
pub trait Endpoint: Send + Sync {
    /// 节点内稳定 id（"screen:0" / "mic:builtin" / "file:notes.txt"）。
    fn id(&self) -> &str;
    fn kind(&self) -> MediaKind;
    /// 用户可见名。
    fn name(&self) -> &str;
    /// 目标类型（确定目标 / 实时目标）：决定默认传输与共享生命周期。
    fn target(&self) -> TargetKind;
    /// 能否被挂载成节点（load 探测结果）。
    fn available(&self) -> bool;
    /// load/share 失败原因。
    fn last_error(&self) -> Option<&str>;
    /// load：探测自身可用性；失败 → `available=false` + 记录 `last_error`。
    fn load(&mut self) -> StdResult<(), String>;
    /// share：订阅达成后启动共享（内部自行 spawn 异步推流）。
    fn share(&self, app: Arc<Kernel>, ctx: SubscribeCtx);
}

// ---------------------------------------------------------------------------
// 具体端点：实时目标（媒体类）
// ---------------------------------------------------------------------------

/// 屏幕端点（实时目标）：load 探测采集可用性（bridge 注入探测闭包——
/// 无图形会话 / ffmpeg 缺失时标记不可挂载，屏幕获取失败前置化）。
pub struct ScreenEndpoint {
    base: EndpointBase,
    probe: Probe,
}

impl ScreenEndpoint {
    /// `probe`：平台适应层注入的屏幕采集可用性探测。
    pub fn new(name: impl Into<String>, probe: Probe) -> Self {
        Self {
            base: EndpointBase {
                id: "screen:0".into(),
                kind: MediaKind::Screen,
                name: name.into(),
                available: false,
                last_error: None,
            },
            probe,
        }
    }
}

impl Endpoint for ScreenEndpoint {
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
        match (self.probe)() {
            Ok(()) => {
                self.base.available = true;
                self.base.last_error = None;
                Ok(())
            }
            Err(e) => {
                self.base.mark_failed(e.clone());
                Err(e)
            }
        }
    }
    fn share(&self, app: Arc<Kernel>, ctx: SubscribeCtx) {
        spawn_media_share(
            app,
            ctx,
            self.name().to_string(),
            Some(VideoSource::Screen),
            None,
        );
    }
}

/// 麦克风端点（实时目标）：load 探测音频采集可用性。
pub struct MicEndpoint {
    base: EndpointBase,
    probe: Probe,
}

impl MicEndpoint {
    pub fn new(name: impl Into<String>, probe: Probe) -> Self {
        Self {
            base: EndpointBase {
                id: "mic:builtin".into(),
                kind: MediaKind::Mic,
                name: name.into(),
                available: false,
                last_error: None,
            },
            probe,
        }
    }
}

impl Endpoint for MicEndpoint {
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
        match (self.probe)() {
            Ok(()) => {
                self.base.available = true;
                self.base.last_error = None;
                Ok(())
            }
            Err(e) => {
                self.base.mark_failed(e.clone());
                Err(e)
            }
        }
    }
    fn share(&self, app: Arc<Kernel>, ctx: SubscribeCtx) {
        spawn_media_share(
            app,
            ctx,
            self.name().to_string(),
            None,
            Some(AudioSourceConfig::default()),
        );
    }
}

/// 系统声音端点（实时目标）：load 探测系统声音采集可用性。
pub struct SystemAudioEndpoint {
    base: EndpointBase,
    probe: Probe,
}

impl SystemAudioEndpoint {
    pub fn new(name: impl Into<String>, probe: Probe) -> Self {
        Self {
            base: EndpointBase {
                id: "sysaudio:builtin".into(),
                kind: MediaKind::SystemAudio,
                name: name.into(),
                available: false,
                last_error: None,
            },
            probe,
        }
    }
}

impl Endpoint for SystemAudioEndpoint {
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
        match (self.probe)() {
            Ok(()) => {
                self.base.available = true;
                self.base.last_error = None;
                Ok(())
            }
            Err(e) => {
                self.base.mark_failed(e.clone());
                Err(e)
            }
        }
    }
    fn share(&self, app: Arc<Kernel>, ctx: SubscribeCtx) {
        let device = stross_media::devices::list_system_audio()
            .into_iter()
            .next();
        let audio = Some(AudioSourceConfig {
            system_audio: device,
            ..Default::default()
        });
        spawn_media_share(app, ctx, self.name().to_string(), None, audio);
    }
}

// ---------------------------------------------------------------------------
// 具体端点：确定目标（文件）
// ---------------------------------------------------------------------------

/// 文件端点（确定目标）：load 探测文件可读；share 一次性推送（传完回 Idle）。
///
/// 本地路径只存在于端点对象内（**绝不出现在 wire / 目录 / 摘要**）。
pub struct FileEndpoint {
    base: EndpointBase,
    path: PathBuf,
}

impl FileEndpoint {
    pub fn new(endpoint_id: String, name: String, path: PathBuf) -> Self {
        Self {
            base: EndpointBase {
                id: endpoint_id,
                kind: MediaKind::File,
                name,
                available: false,
                last_error: None,
            },
            path,
        }
    }
}

impl Endpoint for FileEndpoint {
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
        TargetKind::Determined
    }
    fn available(&self) -> bool {
        self.base.available
    }
    fn last_error(&self) -> Option<&str> {
        self.base.last_error.as_deref()
    }
    fn load(&mut self) -> StdResult<(), String> {
        if self.path.is_file() {
            self.base.available = true;
            self.base.last_error = None;
            Ok(())
        } else {
            let e = format!("文件不可读: {}", self.path.display());
            self.base.mark_failed(e.clone());
            Err(e)
        }
    }
    fn share(&self, app: Arc<Kernel>, ctx: SubscribeCtx) {
        let path = self.path.clone();
        let name = self.name().to_string();
        let endpoint_id = self.id().to_string();
        tokio::spawn(async move {
            let Some(url) = resolve_file_url(&app, &ctx) else {
                tracing::warn!(
                    "文件端点 {endpoint_id} 无可用推送地址（pull 未锚定中继 / push 缺订阅方地址）"
                );
                return;
            };
            let watcher_base = resolve_watcher_base(&app, &ctx);
            let opts = FilePushOptions {
                push_url: url,
                stream_id: ctx.stream_id.clone(),
                title: format!("文件 {name}"),
                share_token: if ctx.delivery == Delivery::Push {
                    ctx.share_token.clone()
                } else {
                    None
                },
                watcher_base,
            };
            match push_file(&path, &opts).await {
                Ok(sent) => tracing::info!(
                    "文件端点 {endpoint_id} 已推送「{name}」({sent} 字节, stream={}) 给订阅方 {}",
                    ctx.stream_id,
                    ctx.subscriber,
                ),
                Err(e) => tracing::warn!(
                    "文件端点 {endpoint_id} 推送失败（订阅方 {}）: {e:#}",
                    ctx.subscriber
                ),
            }
        });
    }
}

// ---------------------------------------------------------------------------
// 共享辅助：媒体推流 / 推送地址
// ---------------------------------------------------------------------------

/// 媒体端点自动推流（实时目标共用）：pull 推本机中继（地址自动），
/// push 凭订阅方凭证出站推入订阅方中继（复用既有 B2 路径）。
fn spawn_media_share(
    app: Arc<Kernel>,
    ctx: SubscribeCtx,
    title: String,
    video: Option<VideoSource>,
    audio: Option<AudioSourceConfig>,
) {
    tokio::spawn(async move {
        let cfg = StreamConfig {
            stream_id: ctx.stream_id.clone(),
            title,
            video,
            quality: Quality::MEDIUM,
            audio,
            duration_secs: None,
            share_token: if ctx.delivery == Delivery::Push {
                ctx.share_token.clone()
            } else {
                None
            },
        };
        let relay_url = resolve_media_url(&ctx);
        match app.start_stream(cfg, relay_url).await {
            Ok(r) => tracing::info!(
                "端点已自动推流: stream={} 订阅方 {}",
                r.stream_id,
                ctx.subscriber
            ),
            Err(e) => tracing::warn!("端点自动推流失败（订阅方 {}）: {e:#}", ctx.subscriber),
        }
    });
}

/// 媒体推流的目标地址：push → 订阅方中继 + `/ws/push`；pull → `None`
/// （推本机中继，地址由内核自动选择）。
fn resolve_media_url(ctx: &SubscribeCtx) -> Option<String> {
    if ctx.delivery == Delivery::Push {
        let base = ctx.relay_addr.as_deref()?;
        Some(format!("{base}/ws/push"))
    } else {
        None
    }
}

/// 文件泵推送地址：push → 订阅方中继；pull → 自己的受控中继（回环地址）。
fn resolve_file_url(app: &Kernel, ctx: &SubscribeCtx) -> Option<String> {
    match ctx.delivery {
        Delivery::Push => {
            let base = ctx.relay_addr.as_deref()?;
            Some(format!("{base}/ws/push"))
        }
        Delivery::Pull | Delivery::Both => {
            let port = app.relay_port()?;
            Some(format!("ws://127.0.0.1:{port}/ws/push"))
        }
    }
}

/// 观看数轮询基址（文件泵等观看者接入用）：push = 订阅方中继；pull = 自己中继。
fn resolve_watcher_base(app: &Kernel, ctx: &SubscribeCtx) -> Option<String> {
    match ctx.delivery {
        Delivery::Push => ctx.relay_addr.clone(),
        Delivery::Pull | Delivery::Both => app.relay_port().map(|p| format!("ws://127.0.0.1:{p}")),
    }
}

// ---------------------------------------------------------------------------
// 订阅事件 / 文件源
// ---------------------------------------------------------------------------

/// 订阅事件载荷：端点 `share` 开推的依据（协商层授予成功后构造，
/// docs/endpoint-model.md §5 联动）。
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

/// 文件端点本地文件源（`control.rs` 状态展示用；路径不落 wire）。
#[derive(Debug, Clone)]
pub struct FileSource {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
}

// ---------------------------------------------------------------------------
// 注册表（单层：一张端点表）
// ---------------------------------------------------------------------------

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn ok_probe() -> Probe {
        Arc::new(|| Ok(()))
    }

    fn fail_probe(reason: &'static str) -> Probe {
        let r = reason.to_string();
        Arc::new(move || Err(r.clone()))
    }

    fn screen() -> Box<dyn Endpoint> {
        Box::new(ScreenEndpoint::new("屏幕", ok_probe()))
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
        assert!(r2.seed(Box::new(ScreenEndpoint::new(
            "屏幕",
            fail_probe("无图形会话（DISPLAY / WAYLAND_DISPLAY 均未设置）")
        ))));
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
        assert_eq!(m.endpoint_id, m.endpoint_id, "动态端点");
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
            base: EndpointBase,
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
            fn share(&self, _app: Arc<Kernel>, ctx: SubscribeCtx) {
                assert_eq!(ctx.subscriber, "dev-phone");
                self.fired.fetch_add(1, Ordering::SeqCst);
            }
        }
        let mut r = EndpointRegistry::new();
        r.seed(Box::new(CountingEndpoint {
            base: EndpointBase {
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
