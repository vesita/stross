//! 引导层（docs/endpoint-model-v2.md §4）：节点间「链接建立」的编排门面。
//!
//! 框架语义（程序负责相互发现，**引导模块负责引导链接建立**）：
//!
//! 1. **相互嗅探**：锚定受控中继并广播 mDNS **L1 摘要**（节点 → 端点清单，
//!    `DiscoveryInfo v3.endpoints`）；
//! 2. **基础通信链接**：受控中继（数据面入口）+ 协商端点 18779
//!    （目录 **L2** 与订阅握手）；
//! 3. **目录与订阅**：`GET /api/endpoints`（可订阅端点清单）+
//!    `POST /api/negotiator/request`（订阅握手，见 [`ShareNegotiator`]）。
//!
//! 数据面与传输（SRT/QUIC/WS/WebRTC）**不在此层**；端点传输协议由公开者
//! 在 [`EndpointManifest`] 里声明（docs/endpoint-model-v2.md）。
//!
//! CLI serve 与 GUI 桌面共用这套启动原语：CLI 用完整组合 [`start`]；
//! GUI 桌面按自身生命周期分步（setup 只起目录/握手，锚定由前端触发，
//! Android 不起协商端点、仅作客户端）。
//!
//! 平台适应（base_dir / hostname）一律由调用方经 [`stross_bridge`] 注入——
//! 内核零路径解析、零 OS 调用。

use std::path::Path;
use std::sync::Arc;

use crate::relay::DEFAULT_PORT;

use crate::DEFAULT_NEGOTIATOR_PORT;
use crate::negotiator::{NegotiatorUi, ShareNegotiator, load_or_create_identity};
use crate::{Kernel, RelayInfo};

/// 引导结果：基础通信链接（受控中继 + mDNS L1）与目录/握手端点。
pub struct Bootstrap {
    pub relay: RelayInfo,
    negotiator: Arc<ShareNegotiator>,
}

impl Bootstrap {
    /// 目录/握手端点句柄（交给 CtrlServer 等持有）。
    pub fn negotiator(&self) -> Arc<ShareNegotiator> {
        self.negotiator.clone()
    }
}

/// 设备默认名（内核无法取 OS 主机名时的回退；壳层需要真实主机名时经
/// [`stross_bridge::hostname`] 注入后调用 [`ensure_identity`]）。
pub const DEFAULT_NODE_NAME: &str = "Stross 设备";

/// 注入本机持久化身份（mDNS 实例名携带 device_id 前缀防同名互覆盖；
/// GUI 与 CLI 共用同一 identity.json；已注入时为空操作）。
///
/// `hostname`：身份默认名（壳层经 [`stross_bridge::hostname`] 注入）。
pub fn ensure_identity(kernel: &Kernel, base_dir: &Path, hostname: &str) {
    if kernel.device_identity().is_some() {
        return;
    }
    kernel.set_identity(load_or_create_identity(base_dir, hostname));
}

/// 第 1/2 步（互相嗅探 + 基础通信链接）：锚定受控中继并广播 mDNS L1 摘要。
///
/// 端口语义沿用 serve：中继 HTTP/WS 端口（0 = 随机，被占用回退随机）；
/// SRT/QUIC 固定端口便于防火墙只放行已知端口。
/// `hostname`：mDNS 广播主机名（调用方注入，内核零 OS 调用）。
pub async fn anchor(
    kernel: Arc<Kernel>,
    relay_port: u16,
    srt_port: u16,
    quic_port: u16,
    hostname: &str,
) -> anyhow::Result<RelayInfo> {
    let port = if relay_port == 0 {
        DEFAULT_PORT
    } else {
        relay_port
    };
    kernel
        .start_relay_fixed(port, srt_port, quic_port, hostname)
        .await
        .map_err(|e| anyhow::anyhow!(e))
}

/// 第 3 步（目录 + 订阅握手）：启动协商端点（LAN 可达，CORS 放行）。
///
/// 订阅联动由端点**自驱动**（`share` 契约，docs/endpoint-model-v2.md §3）——
/// 协商层授予成功后直接调用端点 share，无需壳层接线（曾需安装订阅驱动，
/// GUI 漏装导致"订阅了不推流"；契约化后行为天然一致）。
///
/// `port = 0` 理解为默认端口 [`DEFAULT_NEGOTIATOR_PORT`]（与本机中继端口
/// 语义一致：0 = 常规部署端口）。本地双端测试用显式端口避免冲突。
pub async fn start_handshake_on(
    kernel: Arc<Kernel>,
    ui: Arc<dyn NegotiatorUi>,
    base_dir: &Path,
    port: u16,
) -> anyhow::Result<Arc<ShareNegotiator>> {
    let port = if port == 0 {
        DEFAULT_NEGOTIATOR_PORT
    } else {
        port
    };
    Ok(Arc::new(
        ShareNegotiator::start(kernel, ui, base_dir, port)
            .await
            .map_err(|e| anyhow::anyhow!("启动协商端点失败: {e}"))?,
    ))
}

/// 默认端口版（GUI 桌面等固定 18779 的调用方用）。
pub async fn start_handshake(
    kernel: Arc<Kernel>,
    ui: Arc<dyn NegotiatorUi>,
    base_dir: &Path,
) -> anyhow::Result<Arc<ShareNegotiator>> {
    start_handshake_on(kernel, ui, base_dir, DEFAULT_NEGOTIATOR_PORT).await
}

/// 完整引导（CLI serve 等常驻实例）：身份 → 锚定（含 L1 广播）→
/// 目录/握手端点（订阅驱动在 `start_handshake_on` 内默认安装）。
///
/// `hostname`：mDNS 广播主机名（调用方注入）。
#[allow(clippy::too_many_arguments)] // 启动原语一次性组合全部入口参数，保持扁平
pub async fn start(
    kernel: Arc<Kernel>,
    ui: Arc<dyn NegotiatorUi>,
    base_dir: &Path,
    relay_port: u16,
    srt_port: u16,
    quic_port: u16,
    negotiator_port: u16,
    hostname: &str,
) -> anyhow::Result<Bootstrap> {
    let relay = anchor(kernel.clone(), relay_port, srt_port, quic_port, hostname).await?;
    let negotiator = start_handshake_on(kernel, ui, base_dir, negotiator_port).await?;
    Ok(Bootstrap { relay, negotiator })
}
