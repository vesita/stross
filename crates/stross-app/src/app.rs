//! 核心封装模块：应用级状态机与命令面。
//!
//! [`StrossApp`] 把共享模块（中继/推流客户端/发现）与系统适配模块（采集后端）
//! 组合成"先连接、再推流/观看"的完整应用逻辑，**不依赖任何 UI 框架**：
//!
//! * 无 Tauri / web 依赖，可在纯 Rust 环境下单元测试
//! * UI 层（桌面 / Android）只负责把命令转发到这里
//! * 采集后端通过 [`Arc<dyn CaptureBackend>`] 注入，平台差异被隔离在
//!   `stross-media`（桌面 ffmpeg）与 UI 层（Android 原生）实现里

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use stross_core::discovery::Discovery;
use stross_core::net::local_ips;
use stross_core::relay::{DEFAULT_PORT, RelayHandle, RelayServer};
use stross_media::capture::CaptureBackend;
use stross_media::pipeline::{StreamConfig, ffmpeg_available};

use crate::engine::SenderEngine;
use crate::kernel::{Kernel, NodeInfo, NodeRole};

/// 运行平台（UI 层注入）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Desktop,
    Android,
}

impl Platform {
    pub fn as_str(&self) -> &'static str {
        match self {
            Platform::Desktop => "desktop",
            Platform::Android => "android",
        }
    }
}

/// 应用全局状态。
pub struct StrossApp {
    platform: Platform,
    engine: Mutex<Option<RunningStream>>,
    /// 常驻本机中继（连接阶段启动，观看与推流共用）。
    relay: Mutex<Option<RelayHandle>>,
    /// mDNS 广播句柄（本机中继启动时广播，便于局域网内设备扫描发现）。
    discovery: Mutex<Option<Discovery>>,
    /// 采集后端（平台相关，UI 层注入；`Arc` 使其可被引擎复用）。
    backend: Mutex<Option<Arc<dyn CaptureBackend>>>,
    /// 内核（控制面）：设备图 / 会话管理 / 路由（设计文档 §3）。
    kernel: Kernel,
}

/// 运行中的推流。
struct RunningStream {
    engine: SenderEngine,
    relay_port: u16,
    title: String,
    stream_id: String,
    started_at: u64,
}

impl StrossApp {
    pub fn new(platform: Platform) -> Self {
        Self {
            platform,
            engine: Mutex::new(None),
            relay: Mutex::new(None),
            discovery: Mutex::new(None),
            backend: Mutex::new(None),
            kernel: Kernel::new(),
        }
    }

    /// 内核（控制面）引用。
    pub fn kernel(&self) -> &Kernel {
        &self.kernel
    }

    /// 注入采集后端（UI 层在启动时调用一次）。
    pub fn set_backend(&self, backend: Arc<dyn CaptureBackend>) {
        *self.backend.lock().unwrap() = Some(backend);
    }

    // -----------------------------------------------------------------------
    // 信息与设备
    // -----------------------------------------------------------------------

    /// 应用信息（版本 / 平台 / ffmpeg 是否可用 / 本机 IP）。
    pub fn app_info(&self) -> AppInfo {
        AppInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            platform: self.platform.as_str().to_string(),
            ffmpeg: ffmpeg_available(),
            ips: local_ips().into_iter().map(|ip| ip.to_string()).collect(),
        }
    }

    /// 摄像头 / 麦克风 / 系统声音设备列表。
    pub fn list_devices(&self) -> DeviceList {
        DeviceList {
            cameras: stross_media::devices::list_cameras(),
            audio_inputs: stross_media::devices::list_audio_inputs(),
            system_audio: stross_media::devices::list_system_audio(),
        }
    }

    // -----------------------------------------------------------------------
    // 连接阶段
    // -----------------------------------------------------------------------

    /// 启动/复用本机中继（"先连接"步骤的本机选项）。
    pub async fn start_relay(&self) -> Result<RelayInfo, String> {
        {
            let guard = self.relay.lock().unwrap();
            if let Some(r) = guard.as_ref() {
                return Ok(relay_info(r.port));
            }
        }
        let handle = RelayServer::start(DEFAULT_PORT)
            .await
            .map_err(|e| e.to_string())?;
        let port = handle.port;
        *self.relay.lock().unwrap() = Some(handle);
        // 把本机注册进内核设备图（含采集能力，供会话协商）
        self.register_local_node();
        // mDNS 广播本机中继，局域网内其它设备（如电脑端 Stross）可扫描发现
        if let Some(ip) = local_ips().into_iter().next() {
            let instance = format!("sender-{port}");
            match Discovery::start(
                &instance,
                ip,
                port,
                &[
                    ("kind", "relay"),
                    ("name", "Stross 本机中继"),
                    ("roles", "sender,viewer,relay"),
                    ("transports", "ws,webrtc,srt,quic"),
                    ("codecs", "h264,aac"),
                ],
            ) {
                Ok(d) => {
                    *self.discovery.lock().unwrap() = Some(d);
                }
                Err(e) => tracing::warn!("mDNS 广播失败: {e}"),
            }
        }
        Ok(relay_info(port))
    }

    /// 把本机节点（含采集能力）注册进内核设备图。
    pub fn register_local_node(&self) {
        let kernel = &self.kernel;
        kernel.upsert_node(NodeInfo {
            node_id: "local".into(),
            name: hostname::get()
                .map(|h| h.to_string_lossy().to_string())
                .unwrap_or_else(|_| "本机".into()),
            roles: vec![NodeRole::Sender, NodeRole::Viewer, NodeRole::Relay],
            caps: vec![],
            endpoints: vec![],
        });
        if let Some(backend) = self.backend.lock().unwrap().as_ref() {
            kernel.register_capability("local", backend.descriptor());
        }
    }

    /// mDNS 扫描局域网内的其它中继。
    ///
    /// 返回的 [`RelayInfo`] 透传 mDNS TXT 信息（设备名 / 角色 / 传输），
    /// 供前端直接展示设备卡片，无需再手动输入地址。
    pub async fn scan_relays(&self) -> Result<Vec<RelayInfo>, String> {
        let found = Discovery::browse(Duration::from_secs(2))
            .await
            .map_err(|e| e.to_string())?;
        Ok(found
            .into_iter()
            .map(|d| {
                let txt = |k: &str| {
                    d.txt
                        .iter()
                        .find(|(key, _)| key == k)
                        .map(|(_, v)| v.clone())
                };
                let split = |k: &str| -> Vec<String> {
                    txt(k)
                        .map(|v| {
                            v.split(',')
                                .map(|s| s.trim().to_string())
                                .filter(|s| !s.is_empty())
                                .collect()
                        })
                        .unwrap_or_default()
                };
                let url = format!("http://{}:{}/", d.ip, d.port);
                RelayInfo {
                    port: d.port,
                    urls: vec![url],
                    name: txt("name"),
                    kind: txt("kind"),
                    roles: split("roles"),
                    transports: split("transports"),
                    ip: Some(d.ip.to_string()),
                }
            })
            .collect())
    }

    // -----------------------------------------------------------------------
    // 推流
    // -----------------------------------------------------------------------

    /// 开始推流。
    ///
    /// * `cfg`：采集配置（视频源 / 画质 / 音频）
    /// * `relay_url`：`Some` 推到指定中继（连接阶段得到的 ws 地址）；
    ///   `None` 推到常驻本机中继
    pub async fn start_stream(
        &self,
        cfg: StreamConfig,
        relay_url: Option<String>,
    ) -> Result<StartResult, String> {
        if self.engine.lock().unwrap().is_some() {
            return Err("已经在推流中，请先停止".into());
        }
        let backend = self
            .backend
            .lock()
            .unwrap()
            .clone()
            .ok_or("采集后端未初始化")?;
        // 未指定中继时，推到已连接（常驻）的本机中继
        let relay_url = match relay_url {
            Some(u) => Some(u),
            None => {
                let guard = self.relay.lock().unwrap();
                guard
                    .as_ref()
                    .map(|r| format!("ws://127.0.0.1:{}/ws/push", r.port))
            }
        };
        let engine = SenderEngine::start(cfg.clone(), backend, relay_url.clone(), DEFAULT_PORT)
            .await
            .map_err(|e| e.to_string())?;
        // 有效中继端口：内嵌中继 > 常驻中继 > 默认端口
        let relay_port = engine
            .relay_port()
            .or_else(|| self.relay.lock().unwrap().as_ref().map(|r| r.port))
            .unwrap_or(DEFAULT_PORT);
        let started_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        *self.engine.lock().unwrap() = Some(RunningStream {
            engine,
            relay_port,
            title: cfg.title.clone(),
            stream_id: cfg.stream_id.clone(),
            started_at,
        });
        Ok(StartResult {
            relay_port,
            watch_urls: watch_urls(relay_url.as_deref(), relay_port),
        })
    }

    /// 停止推流。
    pub async fn stop_stream(&self) -> Result<(), String> {
        let engine = self.engine.lock().unwrap().take();
        if let Some(stream) = engine {
            tokio::spawn(async move {
                stream.engine.stop().await;
            });
        }
        Ok(())
    }

    /// 推流状态。
    pub fn stream_status(&self) -> StreamStatus {
        let guard = self.engine.lock().unwrap();
        match guard.as_ref() {
            Some(s) => StreamStatus {
                running: true,
                stream_id: Some(s.stream_id.clone()),
                title: Some(s.title.clone()),
                relay_port: Some(s.relay_port),
                started_at: Some(s.started_at),
            },
            None => StreamStatus {
                running: false,
                stream_id: None,
                title: None,
                relay_port: None,
                started_at: None,
            },
        }
    }

    /// 采集真实状态（Android 由原生控制帧异步回报；桌面在启动后即为就绪）。
    pub fn capture_status(&self) -> CaptureStatusView {
        let active = self.engine.lock().unwrap().is_some();
        let (started, error) = match self.engine.lock().unwrap().as_ref() {
            Some(s) => {
                let st = s.engine.capture_status();
                (st.started, st.error)
            }
            None => (false, None),
        };
        CaptureStatusView {
            active,
            started,
            error,
        }
    }

    /// 运行中推流的中继端口（供"打开观看端"使用）。
    pub fn stream_relay_port(&self) -> u16 {
        self.engine
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.relay_port)
            .unwrap_or(DEFAULT_PORT)
    }
}

// ---------------------------------------------------------------------------
// 值类型
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub version: String,
    /// "desktop" | "android"
    pub platform: String,
    pub ffmpeg: bool,
    pub ips: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceList {
    pub cameras: Vec<stross_media::devices::CameraDevice>,
    pub audio_inputs: Vec<String>,
    pub system_audio: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayInfo {
    pub port: u16,
    pub urls: Vec<String>,
    /// 设备名（mDNS TXT `name`；本机中继或缺失时为 `None`）。
    pub name: Option<String>,
    /// 类型（mDNS TXT `kind`：relay / sender / …）。
    pub kind: Option<String>,
    /// 角色（mDNS TXT `roles`：sender / viewer / relay）。
    pub roles: Vec<String>,
    /// 支持的传输（mDNS TXT `transports`：ws / webrtc / srt / quic）。
    pub transports: Vec<String>,
    /// 中继 IP（本机中继时为 `None`，用 urls 展示）。
    pub ip: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartResult {
    pub relay_port: u16,
    pub watch_urls: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamStatus {
    pub running: bool,
    pub stream_id: Option<String>,
    pub title: Option<String>,
    pub relay_port: Option<u16>,
    pub started_at: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureStatusView {
    pub active: bool,
    pub started: bool,
    pub error: Option<String>,
}

fn relay_info(port: u16) -> RelayInfo {
    let ips = local_ips();
    let mut urls: Vec<String> = ips
        .iter()
        .map(|ip| format!("http://{ip}:{port}/"))
        .collect();
    if urls.is_empty() {
        urls.push(format!("http://127.0.0.1:{port}/"));
    }
    RelayInfo {
        port,
        urls,
        name: Some("Stross 本机中继".into()),
        kind: Some("relay".into()),
        roles: vec!["sender".into(), "viewer".into(), "relay".into()],
        transports: vec!["ws".into(), "webrtc".into(), "srt".into(), "quic".into()],
        ip: None,
    }
}

/// 局域网可访问的观看地址。
///
/// * 推到外部中继（`relay_url` 非回环地址）→ 直接指向该中继
/// * 本机中继（回环地址 / 未指定）→ 列出本机局域网 IP
fn watch_urls(relay_url: Option<&str>, relay_port: u16) -> Vec<String> {
    if let Some(url) = relay_url {
        // ws://host:port/ws/push → http://host:port/
        if let Some(rest) = url
            .strip_prefix("ws://")
            .or_else(|| url.strip_prefix("wss://"))
        {
            let host_port: String = rest.split('/').next().unwrap_or("").to_string();
            if !host_port.starts_with("127.0.0.1")
                && !host_port.starts_with("localhost")
                && !host_port.starts_with("0.0.0.0")
            {
                return vec![format!("http://{host_port}/")];
            }
        }
    }
    relay_info(relay_port).urls
}

#[cfg(test)]
mod tests {
    use super::*;
    use stross_proto::frame::Frame;
    use tokio::sync::mpsc;

    /// 测试用假后端：记录是否被调用。
    struct MockBackend(std::sync::atomic::AtomicBool);
    impl CaptureBackend for MockBackend {
        fn start(&self, _cfg: &StreamConfig, _tx: mpsc::Sender<Frame>) -> anyhow::Result<()> {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        fn stop(&self) {
            self.0.store(false, std::sync::atomic::Ordering::SeqCst);
        }
        fn status(&self) -> stross_media::capture::CaptureStatus {
            stross_media::capture::CaptureStatus {
                started: self.0.load(std::sync::atomic::Ordering::SeqCst),
                error: None,
            }
        }
    }

    #[test]
    fn app_info_and_devices_never_panic() {
        let app = StrossApp::new(Platform::Desktop);
        let info = app.app_info();
        assert_eq!(info.platform, "desktop");
        let _ = app.list_devices();
    }

    #[test]
    fn capture_status_requires_backend() {
        let app = StrossApp::new(Platform::Desktop);
        // 未注入后端时采集状态应为未激活
        let st = app.capture_status();
        assert!(!st.active);
        assert!(!st.started);
    }

    #[test]
    fn set_backend_then_query() {
        let app = StrossApp::new(Platform::Android);
        app.set_backend(Arc::new(MockBackend(std::sync::atomic::AtomicBool::new(
            false,
        ))));
        let st = app.capture_status();
        assert!(!st.active); // 未推流
    }

    #[test]
    fn platform_str() {
        assert_eq!(Platform::Desktop.as_str(), "desktop");
        assert_eq!(Platform::Android.as_str(), "android");
    }
}
