//! 端点 ↔ 内核行为契约（docs/endpoint-model.md）。
//!
//! * **端点** = 节点上可共享的能力实体（屏幕 / 麦克风 / 系统声音 / 文件……）：
//!   自维护「可挂载性」（`available`，load 探测结果）与失败原因（`last_error`）；
//! * **行为契约**：每个端点实现两个约定行为——`load`（探测自身可用性）与
//!   `share`（订阅达成后启动共享推流，类型自决）——内核不做类型分派；
//! * **内核经 [`EndpointApp`] 调度**：端点层不依赖内核，只依赖这个契约接口
//!   （推流 / 中继端口 / 文件泵）；内核实现它，壳层无感。
//!
//! 目标类型（[`TargetKind`]）：确定目标（文件等，一次推送、有完成态、Lossless）
//! 与实时目标（屏幕等，持续推流、Lossy）——差异经目标类型维度 + 各端点实现表达。

use std::path::PathBuf;
use std::result::Result as StdResult;
use std::sync::Arc;

use async_trait::async_trait;

use stross_proto::message::{Delivery, MediaKind, PickRule, ReliabilityProfile};

use crate::file::FilePushOptions;
use crate::pipeline::{AudioSourceConfig, StreamConfig, VideoSource};

/// 目标类型：端点分两类的维度（决定默认传输 Lossless/Lossy 与共享生命周期）。
///
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
    pub(crate) fn mark_failed(&mut self, e: String) {
        self.available = false;
        self.last_error = Some(e);
    }
}

/// 端点 ↔ 内核行为契约。
///
/// 每个端点必须实现两个约定行为（与内核的约定，非语言特性）：
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
    /// 传输层可靠性档案（通信模式 v2，docs/comm-mode-v2.md §3）：协商时随
    /// 清单上报，内核按它装载传输模块。默认按目标类型推断——实时目标
    /// （媒体）Lossy（允许丢包），确定目标（文件等）Lossless（不允许丢包）。
    fn transport_profile(&self) -> ReliabilityProfile {
        match self.target() {
            TargetKind::Live => ReliabilityProfile::Lossy,
            TargetKind::Determined => ReliabilityProfile::Lossless,
        }
    }
    /// pick 规则（装载/解读语义，docs/comm-mode-v2.md §3.0）：协商时随清单
    /// 上报，发送侧装载逻辑与接收侧解读模块共用。默认按目标类型推断——
    /// 实时目标 Realtime（严格即时：低延迟、容忍丢帧），确定目标
    /// StrictOrdered（严格顺序：逐字节不丢）。
    fn pick_rule(&self) -> PickRule {
        match self.target() {
            TargetKind::Live => PickRule::Realtime,
            TargetKind::Determined => PickRule::StrictOrdered,
        }
    }
    /// 能否被挂载成节点（load 探测结果）。
    fn available(&self) -> bool;
    /// load/share 失败原因。
    fn last_error(&self) -> Option<&str>;
    /// load：探测自身可用性；失败 → `available=false` + 记录 `last_error`。
    fn load(&mut self) -> StdResult<(), String>;
    /// share：订阅达成后启动共享（内部自行 spawn 异步推流）。
    fn share(&self, app: Arc<dyn EndpointApp>, ctx: SubscribeCtx);
}

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
    /// 协商定稿的传输层可靠性档案（允许丢包/不允许丢包/自适应）；
    /// 端点据此装载对应传输模块（通信模式 v2，docs/comm-mode-v2.md §3）。
    pub transport_profile: ReliabilityProfile,
    /// 协商定稿的 pick 规则（严格即时/严格顺序）；端点据此装载对应
    /// 装载/解读模块（发送侧装载逻辑与接收侧解读模块共用同一规则）。
    pub pick_rule: PickRule,
    /// push 模式：订阅方中继 HTTP 基址（`ws://ip:port`；公开方出站 push 目标）。
    pub relay_addr: Option<String>,
    /// push 模式：订阅方自签的一次性接入凭证（推流 Hello 出示）。
    pub share_token: Option<String>,
}

/// 端点可见的内核调度能力（内核实现；端点层只依赖此契约，不依赖内核类型）。
///
/// 内核 = 纯管理调度：会话 / 鉴权 / 路由 / 注册表。数据面的执行细节
/// （ffmpeg 子进程、portal+pipewire 采集、文件泵）都在端点层。
#[async_trait]
pub trait EndpointApp: Send + Sync {
    /// 启动一次媒体推流（pull = 推本机中继，`relay_url=None`；push = 出站）。
    async fn start_stream(
        &self,
        cfg: StreamConfig,
        relay_url: Option<String>,
    ) -> anyhow::Result<stross_types::StartResult>;
    /// 内嵌中继端口（未锚定/未启动时为 `None`）。
    fn relay_port(&self) -> Option<u16>;
    /// 文件泵推送（文件端点确定目标的一次推送；返回发送字节数）。
    async fn push_file(&self, path: PathBuf, opts: FilePushOptions) -> anyhow::Result<u64>;
    /// 端点共享登记：媒体端点 `start_stream` 成功后回调（实时目标生命周期治理用——
    /// watchers=0 自动收尾 / 取消通告联动停止 / 同端点订阅收敛）。
    ///
    /// `self_weak`：发起共享的内核弱引用（生命周期治理任务在无观看者时经它回调
    /// [`Self::stop_share_if_unwatched`]，避免任务持有强引用拖住内核）。
    /// 默认空实现：不登记的端点（文件等有完成态）不受 watchers 自动停止影响。
    fn note_share_active(
        &self,
        _self_weak: std::sync::Weak<dyn EndpointApp>,
        _endpoint_id: &str,
        _stream_id: &str,
        _delivery: Delivery,
    ) {
    }
    /// 端点共享停止回调：watchers 归零复查确认无人观看后调用（默认空实现）。
    fn stop_share_if_unwatched(&self, _stream_id: &str) {}
}

/// 媒体端点自动推流（实时目标共用）：pull 推本机中继（地址自动），
/// push 凭订阅方凭证出站推入订阅方中继。
///
/// `endpoint_id`：端点身份（共享登记用；见 [`EndpointApp::note_share_active`]）。
pub fn spawn_media_share(
    app: Arc<dyn EndpointApp>,
    ctx: SubscribeCtx,
    endpoint_id: &str,
    title: String,
    video: Option<VideoSource>,
    audio: Option<AudioSourceConfig>,
) {
    let endpoint_id = endpoint_id.to_string();
    let self_weak = std::sync::Arc::downgrade(&app);
    tokio::spawn(async move {
        let cfg = StreamConfig {
            stream_id: ctx.stream_id.clone(),
            title,
            video,
            quality: crate::pipeline::Quality::MEDIUM,
            audio,
            duration_secs: None,
            // 订阅驱动定稿（docs/endpoint-model.md §10）：只走 pull——推本机
            // 中继，无出站凭证。
            share_token: None,
        };
        let relay_url = resolve_media_url(&ctx);
        match app.start_stream(cfg, relay_url).await {
            Ok(r) => {
                tracing::info!(
                    "端点已自动推流: stream={} 订阅方 {}",
                    r.stream_id,
                    ctx.subscriber
                );
                // 生命周期治理登记（watchers=0 自动收尾 / 取消通告联动停止 / 订阅收敛）
                app.note_share_active(self_weak, &endpoint_id, &r.stream_id, ctx.delivery);
            }
            Err(e) => tracing::warn!("端点自动推流失败（订阅方 {}）: {e:#}", ctx.subscriber),
        }
    });
}

/// 媒体推流的目标地址：订阅驱动定稿只走 pull → `None`（推本机中继，地址由
/// 内核自动选择；无 push 出站路径）。
pub fn resolve_media_url(_ctx: &SubscribeCtx) -> Option<String> {
    None
}

/// 文件泵推送地址：订阅驱动定稿只走 pull → 自己的受控中继（回环地址）。
pub fn resolve_file_url(app: &dyn EndpointApp, _ctx: &SubscribeCtx) -> Option<String> {
    let port = app.relay_port()?;
    Some(format!("ws://127.0.0.1:{port}/ws/push"))
}

/// 观看数轮询基址（文件泵等观看者接入用）：订阅驱动定稿只走 pull → 自己中继。
pub fn resolve_watcher_base(app: &dyn EndpointApp, _ctx: &SubscribeCtx) -> Option<String> {
    app.relay_port().map(|p| format!("ws://127.0.0.1:{p}"))
}
