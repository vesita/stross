//! 应用契约层（壳层只读；**内核产出，壳层不定义 wire 结构**）。
//!
//! 分层：线协议类型在 stross-proto（设备间 wire 单一真源）；**跨壳层复用的
//! 应用契约**（设备卡片 / 推流状态 / 中继入口 / 控制面载荷 / 协商与订阅 DTO）
//! 收敛在本 crate——依赖只到 stross-proto + serde，**不背内核逻辑**（tokio/axum/
//! mdns 等）。任何只想消费类型的地方（CLI / GUI / 将来的服务端或游戏联机
//! 基础设施）挂本 crate 即可对上契约，无需拖入内核。
//!
//! 约定：
//! * 所有结构 `#[serde(rename_all = "camelCase")]`——serde 字段名即 wire 键，
//!   单一真源（CLI 经 `control::client::request_as` 反序列化，不再手写 JSON
//!   字符串键）；
//! * 与 wire 字符串（"public"/"pull"/"mic"…）对应的枚举 `as_str` / `from_wire`
//!   在 stross-proto（`stross_proto::message`），本 crate 只引用不重定义；
//! * 纯数据 DTO 迁入（`CameraDevice` / `PendingRequest` / `MediaSubscribeOutcome`），
//!   原属 crate 以 `pub use stross_types::Xxx` 重导出保持路径兼容；
//! * 构造帮助函数（`relay_info` / `watch_urls`）留在 stross-kernel 的 `view` 模块。

use serde::{Deserialize, Serialize};

use stross_proto::message::{
    Delivery, EndpointManifest, EndpointSummary, MediaKind, RoleId, TransportId,
};

// ---------------------------------------------------------------------------
// 固定端口真源（跨壳层单一真源；docs/layering-architecture.md 端口约定）
// ---------------------------------------------------------------------------

/// 固定端口真源：库层与壳层一律引用此处，**禁止在局部重复硬编码端口号**
/// （曾因 `relay::DEFAULT_PORT` 与移动端特例混用一个常量导致数据面/协议默认漂移）。
///
/// 中继 HTTP/WS 18777（桌面默认）、控制面 18778（仅回环）、协商 + 发现权威
/// 18779（同一服务，故发现端口 == 协商端口）、SRT 33462、QUIC 33464；
/// 8777 是 Android GUI 的特殊中继入口（与桌面默认分离，勿合并）。
pub mod ports {
    /// 桌面默认中继 HTTP/WS 入口。
    pub const RELAY_HTTP: u16 = 18777;
    /// Android GUI 中继入口（平台特例，勿与 [`RELAY_HTTP`] 合并为一个常量）。
    pub const GUI_RELAY_HTTP: u16 = 8777;
    /// 控制面（仅回环）。
    pub const CTRL: u16 = 18778;
    /// 凭证协商 + 发现权威（同一服务；发现端口即协商端口）。
    pub const NEGOTIATOR_DISCOVERY: u16 = 18779;
    /// SRT 数据面默认端口。
    pub const SRT: u16 = 33462;
    /// QUIC 数据面默认端口。
    pub const QUIC: u16 = 33464;
}

// ---------------------------------------------------------------------------
// 设备 / 采集 DTO
// ---------------------------------------------------------------------------

/// 摄像头设备（采集枚举结果；跨壳层展示用）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CameraDevice {
    /// 稳定标识（Linux 为 `/dev/videoN`，Windows 为 dshow 名称）。
    pub id: String,
    /// 展示名称。
    pub name: String,
}

/// 摄像头 / 麦克风 / 系统声音设备清单。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceList {
    pub cameras: Vec<CameraDevice>,
    pub audio_inputs: Vec<String>,
    pub system_audio: Vec<String>,
}

// ---------------------------------------------------------------------------
// 展示视图（设备卡片 / 推流状态 / 中继入口 / 目录）
// ---------------------------------------------------------------------------

/// 应用信息（版本 / 平台 / ffmpeg 是否可用 / 本机 IP）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub version: String,
    /// "desktop" | "android"
    pub platform: String,
    pub ffmpeg: bool,
    pub ips: Vec<String>,
}

/// 中继入口信息（mDNS 能力引导；本机中继或扫描结果共用）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayInfo {
    pub port: u16,
    pub urls: Vec<String>,
    /// 设备名（mDNS 能力引导 `name`；本机中继或缺失时为 `None`）。
    pub name: Option<String>,
    /// 类型（relay / sender / …）。
    pub kind: Option<String>,
    /// 角色（mDNS 能力引导 `roles`；枚举，序列化与字符串时代一致）。
    pub roles: Vec<RoleId>,
    /// 支持的传输（mDNS 能力引导 `transports`；序列化后与字符串时代一致）。
    pub transports: Vec<TransportId>,
    /// 中继 IP（本机中继时为 `None`，用 urls 展示）。
    pub ip: Option<String>,
    /// 端点框架 L1：该节点公开的端点清单摘要（id/kind/name/是否可挂载/
    /// 是否已通告；本机 = 注册表快照，对端 = mDNS `DiscoveryInfo v3.endpoints` 解码）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub endpoints: Vec<EndpointSummary>,
}

/// 推流启动结果（控制面 `start-stream` 载荷）。
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartResult {
    pub relay_port: u16,
    pub watch_urls: Vec<String>,
    /// 实际流 id（内核签发，D4：与 session id 合一；接收端据此订阅）。
    pub stream_id: String,
}

/// 推流状态。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamStatus {
    pub running: bool,
    pub stream_id: Option<String>,
    pub title: Option<String>,
    pub relay_port: Option<u16>,
    pub started_at: Option<u64>,
}

/// 采集真实状态视图。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureStatusView {
    pub active: bool,
    pub started: bool,
    pub error: Option<String>,
}

/// 本机目录（全部端点；节点卡片端点树渲染用）。
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalCatalog {
    pub endpoints: Vec<EndpointManifest>,
}

/// 手机麦克风接入凭证视图（B2：电脑端签发后展示给手机）。
///
/// 线协议类型已收敛至 stross-proto：此处重导出保持
/// `stross_types::ShareTokenView` / `stross_kernel::ShareTokenView`
/// 路径兼容（GUI 命令层在用）。
pub use stross_proto::message::ShareTokenView;

// ---------------------------------------------------------------------------
// 协商 / 订阅 DTO
// ---------------------------------------------------------------------------

/// 待人工确认的请求（推送给 UI 展示；控制面载荷经 `request_as` 反序列化）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingRequest {
    /// 挂起请求 id（`negotiator_respond` 时回填）。
    pub id: String,
    pub device_id: String,
    pub device_name: String,
    /// 序列化后的媒体名（`MediaKind` camelCase；前端展示用）。
    pub media: Vec<String>,
    /// 订阅目标端点名（端点语义；旧语义为 `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_name: Option<String>,
    pub created_at: u64,
}

/// 媒体端点订阅结果（GUI 命令 / 未来 CLI 共用）：握手后交给既有接收链路
/// `start_receive(relay_url, stream_id)` 实际观看 / 播放。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaSubscribeOutcome {
    /// 公开方拍板后的方向（pull = 连公开方中继；push = 公开方推入本机）。
    pub delivery: Delivery,
    /// watch 入口（ws://host:port；pull = 公开方中继，push = 本机中继）。
    pub relay_url: String,
    /// 观看流 id（pull = 公开方会话；push = 本机自签会话）。
    pub stream_id: String,
}

/// 文件接收结果（文件端点半程：握手 → 接收 → 落盘；GUI 命令 / CLI 展示共用，
/// JSON 序列化供前端消费）。应用契约层单一真源（订阅端
/// [`Endpoint::subscribe`] 经 [`EndpointApp::receive_file`] 返回）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceivedFile {
    /// 落盘文件名（已净化，只取 basename）。
    pub name: String,
    /// 文件字节数（与首帧 FileMeta 校验一致）。
    pub size: u64,
    /// 落盘路径。
    pub path: std::path::PathBuf,
}

// ---------------------------------------------------------------------------
// 控制面载荷（CtrlResponse::Ok 的 payload 类型化单一真源；CLI 经
// `control::client::request_as` 反序列化，不再手写 JSON 字符串键）
// ---------------------------------------------------------------------------

/// `ctrl create-session` 载荷。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCreatedView {
    pub session_id: String,
    pub title: String,
}

/// `ctrl authorize` 载荷。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizedView {
    pub session_id: String,
    pub authorized: bool,
}

/// `ctrl teardown` 载荷。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeardownView {
    pub session_id: String,
}

/// `ctrl start-stream` 载荷（复用 [`StartResult`]：relayPort/watchUrls/streamId）。
/// `ctrl stop-stream` 载荷。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoppedView {
    pub stopped: bool,
}

/// `ctrl share-token` 载荷（凭证 JSON + 展示字段；media 为 MediaKind camelCase）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IssuedShareTokenView {
    pub token: String,
    pub stream_id: String,
    pub pin: String,
    pub expires_at: u64,
    pub media: Vec<MediaKind>,
}

/// `ctrl list-sessions` 载荷。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    pub session_id: String,
    pub source: String,
    pub sinks: Vec<String>,
    pub requires_pin: bool,
}

/// `ctrl list-sessions` 响应信封。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionsPayload {
    pub sessions: Vec<SessionView>,
}

/// `ctrl status` 载荷。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusView {
    pub version: String,
    pub platform: String,
    pub uptime_secs: u64,
    pub relay_port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub srt_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quic_port: Option<u16>,
    pub streaming: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_started_at: Option<u64>,
    pub sessions: usize,
}

/// `ctrl negotiator-list` 载荷（pending 直接序列化 [`PendingRequest`]）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingRequestsPayload {
    pub pending: Vec<PendingRequest>,
}

/// `ctrl negotiator-respond` 载荷：允许 → stream_id/pin/expires_at/trusted；
/// 拒绝/请求已失效 → `denied=true`（其余字段省略）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrantResponseView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default)]
    pub trusted: bool,
    #[serde(default)]
    pub denied: bool,
}

/// `ctrl endpoint publish` 载荷（直接序列化 `EndpointManifest`：endpointId/name/delivery…）。
/// 无独立结构——manifest 即载荷（字段名 = wire 键）。
pub type EndpointPublishedView = EndpointManifest;

/// `ctrl endpoint publish-file` 载荷（manifest + 文件字节数）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilePublishedView {
    pub endpoint_id: String,
    pub name: String,
    pub size: u64,
    pub delivery: Delivery,
}

/// `ctrl endpoint unpublish` 载荷。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnpublishedView {
    pub endpoint_id: String,
    pub unpublished: bool,
}

/// `ctrl endpoint list` 载荷（端点清单；含未通告与不可挂载，字段同 manifest）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointListPayload {
    pub endpoints: Vec<EndpointManifest>,
}
