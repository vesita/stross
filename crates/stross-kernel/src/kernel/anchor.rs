//! 内核锚点域（`impl Kernel`）：常驻受控中继 + mDNS 广播 + 统一发现清单。
//!
//! docs/layering-architecture.md：`Kernel` 是全部服务提供的**单一门面**；
//! 本文件把「锚定（start_relay*）/ 可被发现（mDNS）/ 发现清单」这一域的
//! 实现从 `mod.rs` 拆出，方法与公共 API 不变。

use std::sync::Arc;
use std::sync::atomic::Ordering;

use stross_proto::message::{DiscoveryInfo, MediaKind};

use crate::discovery::Discovery;
use crate::error::Result;
use crate::lock::MutexExt;
use crate::relay::{DEFAULT_PORT, RelayServer};
use crate::view;
use crate::{Kernel, RelayInfo};

use super::LocalAnchor;

impl Kernel {
    // -----------------------------------------------------------------------
    // 局域网可发现（mDNS 广播本机：显式用户开关）
    // -----------------------------------------------------------------------

    /// 当前是否可被发现（mDNS 广播本机）。默认关。
    pub fn discoverable(&self) -> bool {
        self.discoverable.load(Ordering::Relaxed)
    }

    /// 启用/关闭可被发现（mDNS 广播本机）。
    ///
    /// 开启时：若已锚定中继，立即广播本机（首次则新建句柄，已广播仅刷新
    /// TXT 摘要——通告状态可能已变）；关闭时：停止本机广播。未锚定仅记状态，
    /// 锚定流程按此状态生效。
    pub fn set_discoverable(&self, on: bool) {
        self.discoverable.store(on, Ordering::Relaxed);
        self.apply_discoverable();
    }

    /// 按当前 `discoverable` 状态收敛 mDNS 广播（锚定 / 端点通告后调用）。
    ///
    /// **锁序**：先取 anchor 锁，再在 [`Self::mdns_info`] 里取 registry 锁。
    /// 反向序（registry → anchor）不存在；锚定流程锚定时不持 registry 锁、
    /// 通告流程只持 registry 锁并在锁外调本方法，故无死锁。
    pub(crate) fn apply_discoverable(&self) {
        let on = self.discoverable.load(Ordering::Relaxed);
        let mut anchor = self.anchor.lock_poisoned();
        let Some(a) = anchor.as_mut() else {
            return; // 未锚定：仅记状态
        };
        if on {
            // 开启：未广播则新建句柄（try_register_mdns 内部构建摘要）；
            // 已广播则重注册刷新 TXT（端点摘要可能已变）。
            if a.discovery.is_none() {
                a.discovery = self.try_register_mdns(&a.hostname, a.port);
            } else if let Some(d) = a.discovery.as_mut()
                && let Err(e) = d.redefine(&self.mdns_info(&a.hostname))
            {
                tracing::warn!("mDNS 刷新失败: {e}");
            }
        } else {
            // 关闭：停止本机广播（句柄 Drop 即反注册）
            if let Some(mut d) = a.discovery.take() {
                d.stop();
            }
        }
    }

    /// 构造本机 mDNS 能力描述（`DiscoveryInfo`；端点摘要取当前注册表快照）。
    pub(crate) fn mdns_info(&self, hostname: &str) -> DiscoveryInfo {
        DiscoveryInfo::relay_default(
            hostname.to_string(),
            vec![
                MediaKind::Screen,
                MediaKind::Camera,
                MediaKind::Mic,
                MediaKind::SystemAudio,
            ],
        )
        .with_endpoints(self.registry.lock_poisoned().summaries())
    }

    /// 注册 mDNS 广播本机中继；失败告警并返回 `None`（中继仍可用）。
    pub(crate) fn try_register_mdns(&self, hostname: &str, port: u16) -> Option<Discovery> {
        let hex = self
            .identity
            .lock_poisoned()
            .as_ref()
            .map(|id| id.node_id.to_hex());
        let instance = relay_mdns_instance(hex.as_deref(), port);
        match Discovery::start(
            &instance,
            &crate::net::local_ips(),
            port,
            &self.mdns_info(hostname),
            hostname,
        ) {
            Ok(d) => {
                tracing::info!("mDNS 广播已开启（可被发现）: {instance}");
                Some(d)
            }
            Err(e) => {
                tracing::warn!("mDNS 广播失败: {e}");
                None
            }
        }
    }

    // -----------------------------------------------------------------------
    // 连接阶段（锚点：先连接，再推流/观看）
    // -----------------------------------------------------------------------

    /// 启动/复用本机中继（"先连接"步骤的本机选项）。
    ///
    /// 本机中继以**受控模式**启动（需求 F2.2「先会话后传输」）：只有内核
    /// 创建的会话 id 才能推流；中继作为数据面后端接入内核，
    /// 流生命周期事件转发为 [`KernelEvent`]。
    pub async fn start_relay(self: &Arc<Self>, hostname: &str) -> Result<RelayInfo> {
        self.start_relay_on(DEFAULT_PORT, hostname).await
    }

    /// 在指定端口启动中继（受内核控制的数据面）；被占用时回退随机端口。
    pub async fn start_relay_on(self: &Arc<Self>, port: u16, hostname: &str) -> Result<RelayInfo> {
        self.start_relay_fixed(port, 0, 0, hostname).await
    }

    /// 在指定端口启动中继，并固定 SRT/QUIC 传输端口（`0` = 随机）。
    ///
    /// 固定端口便于防火墙只放行已知端口（权限自动化）；SRT/QUIC 被占用时
    /// 各自回退随机端口（实际端口经 `/api/info` 可见）。
    ///
    /// `hostname`：mDNS 广播主机名（**调用方注入**——内核零 OS 调用；
    /// 壳层经 [`stross_bridge::hostname`] 取值）。
    pub async fn start_relay_fixed(
        self: &Arc<Self>,
        port: u16,
        srt_port: u16,
        quic_port: u16,
        hostname: &str,
    ) -> Result<RelayInfo> {
        {
            let guard = self.anchor.lock_poisoned();
            if let Some(a) = guard.as_ref() {
                return Ok(view::relay_info(
                    a.port,
                    hostname,
                    self.registry.lock_poisoned().summaries(),
                ));
            }
        } // 优先指定端口；被占用时回退随机端口（本机中继"能用就行"，不因端口冲突失败）
        let handle =
            if let Ok(h) = RelayServer::start_controlled_with(port, srt_port, quic_port).await {
                h
            } else {
                tracing::warn!("端口 {port} 被占用，本机中继回退到随机端口");
                RelayServer::start_controlled(0).await?
            };
        let port = handle.port;
        handle.set_channel_manager(self.channel_manager.clone());
        // 中继接入内核（数据面后端）：订阅流事件、会话预授权
        self.attach_data_plane(Arc::new(crate::kernel::RelayDataPlane::new(&handle)));
        // 把本机注册进内核设备图（含采集能力，供会话协商）
        self.register_local_node(hostname);
        // mDNS 广播本机中继：**仅当「可被发现」开启时**才广播（显式用户开关，
        // 默认关）。开启由 `set_discoverable(true)` 触发（或锚定前已开）。
        // 能力描述统一走 DiscoveryInfo 单 key JSON（F1.2 / 1d）；多网卡广播
        // 全部局域网 IP（Discovery::start 内部处理空列表回退回环），避免只广播
        // 第一个 IP 导致其它网卡网段扫描不到本机。
        let hostname = hostname.to_string();
        let discovery = if self.discoverable.load(Ordering::Relaxed) {
            self.try_register_mdns(&hostname, port)
        } else {
            None
        };
        *self.anchor.lock_poisoned() = Some(LocalAnchor {
            handle,
            discovery,
            port,
            hostname: hostname.clone(),
        });
        Ok(view::relay_info(
            port,
            &hostname,
            self.registry.lock_poisoned().summaries(),
        ))
    }

    /// 把本机节点（含采集能力）注册进内核设备图。
    pub fn register_local_node(&self, hostname: &str) {
        self.upsert_node(super::NodeInfo {
            node_id: stross_proto::message::NodeId::from("local"),
            name: hostname.into(),
            roles: vec![
                super::NodeRole::Sender,
                super::NodeRole::Viewer,
                super::NodeRole::Relay,
            ],
            caps: vec![],
            addrs: vec![],
        });
        if let Some(backend) = self.backend.lock_poisoned().as_ref() {
            self.register_capability(
                &stross_proto::message::NodeId::from("local"),
                backend.descriptor(),
            );
        }
    }

    /// 本机主中继端口（`start_relay` / `start_relay_on` 启动的那个）。
    pub fn relay_port(&self) -> Option<u16> {
        self.anchor.lock_poisoned().as_ref().map(|a| a.port)
    }

    /// 本机中继全部监听端口：`(ws, srt, quic)`（未启动时为 `None`）。
    ///
    /// 防火墙自动放行按实际端口生成规则（SRT/QUIC 固定端口被占用回退随机时
    /// 也能放行真实端口）。
    pub fn relay_ports(&self) -> Option<(u16, Option<u16>, Option<u16>)> {
        self.anchor
            .lock_poisoned()
            .as_ref()
            .map(|a| (a.port, a.handle.srt_port, a.handle.quic_port))
    }

    /// 统一发现清单（`/api/discovery` 数据源，见 [`crate::discovery::DiscoveryResp`]）：
    /// 从当前锚定中继 + 身份 + 能力组装。未锚定（无中继入口）返回 `None`（非可发现节点）。
    /// `name` 用身份名，与 mDNS 广播的展示名一致（mDNS 与子网扫描都指向同一节点）。
    ///
    /// **可被发现门控**：`discoverable == false` 时也返回 `None`——「可被发现」是
    /// 隐私开关，关闭时**所有**发现路径（mDNS 广播 + 子网单播扫描回退）都不可见。
    /// 子网回退主动探测 `18779/api/discovery`，若不此处门控，mDNS 关闭仍会被
    /// 扫描发现，违背隐私优先语义（用户反馈 bug）。
    pub fn discovery_manifest(&self) -> Option<crate::discovery::DiscoveryResp> {
        // 可被发现关闭 → 不对外提供发现清单（含子网单播回退的探测口径）
        if !self.discoverable() {
            return None;
        }
        let (relay_port, srt_port, quic_port) = self.relay_ports()?;
        let identity = self.node_identity()?;
        let info = self.mdns_info(&identity.node_name);
        Some(crate::discovery::DiscoveryResp {
            node_id: identity.node_id,
            name: info.name,
            relay_port,
            srt_port,
            quic_port,
            roles: info.roles,
            media: info.media,
            transports: info.transports,
            endpoints: info.endpoints,
        })
    }
}

/// 本机中继的 mDNS 实例名：携带持久化 `device_id` 前 8 位 + 端口，保证
/// 局域网内多设备同端口广播时实例名唯一（mdns-sd browse 按实例名键控，
/// 同名实例会互相覆盖导致扫描不到，实测）。
///
/// 未注入身份时回退旧格式 `sender-{port}`（兼容无 UI 接入方）。
pub(crate) fn relay_mdns_instance(node_id: Option<&str>, port: u16) -> String {
    match node_id {
        Some(id) if !id.is_empty() => {
            let short = id.chars().take(8).collect::<String>();
            format!("stross-{short}-{port}")
        }
        _ => format!("sender-{port}"),
    }
}
