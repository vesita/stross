//! 发现机制（discovery）子系统 —— **v0.2.0**（本模块契约已封盘）。
//!
//! 职责（一台设备在局域网内的「被发现」与「去发现」全链路）：
//! - **通告**：锚定后 mDNS 广播本机（[`Discovery`] / [`SERVICE_TYPE`]，TXT 携带
//!   [`DiscoveryInfo`](stross_proto::message::DiscoveryInfo) 能力描述）；可被发现由用户显式开关。
//! - **发现**：mDNS 浏览（[`Discovery::browse`]）为主；**子网单播回退**（[`subnet_scan`] /
//!   [`scan_probe_host`]）在 mDNS 零远端（路由只掐下行多播、单播仍通）时兜底。
//! - **聚合**：[`scan`] / [`scan_lan`] / [`probe_base`] 把发现结果 + 手动地址收敛为
//!   [`ScannedDevice`]（去重/排序/在线探测）。
//! - **统一发现清单**：[`DiscoveryResp`]（`/api/discovery` 数据源，经
//!   [`crate::Kernel::discovery_manifest`] 组装）——mDNS 与子网扫描都收敛到
//!   **同一台设备同一个 `relay_port`**（降低用户认知成本，见 docs/iteration-plan.md 第十七轮）。
//!
//! 已知限制（随封盘固化）：
//! - 发现权威端口固定 [`DISCOVERY_PORT`](crate::discovery::DISCOVERY_PORT)=18779；跑在自定义
//!   协商端口或纯中继节点的设备，子网扫描探不到（交给 mDNS）。
//! - `/api/discovery` 免鉴权（LAN 可信模型，任意来源）；后续加固需最小鉴权。
//! - 拉取式新鲜度（无上下线推送）；多网卡设备在 mDNS（按实例名）与子网扫描（按 ip:port）
//!   下去重键不同，可能出现重复卡片。
//!
//! 分层（docs/layering-architecture.md）：全部收敛在 stross-kernel，平台无关；
//! 壳层（CLI/GUI）只做参数转译 + 展示，禁止各自拼装发现/聚合流程。

pub const DISCOVERY_VERSION: &str = "0.2.0";

mod aggregate;
mod mdns;

// mDNS 通告/浏览
pub use mdns::{BROWSE_TIMEOUT, Discovered, Discovery, SERVICE_TYPE};
// 扫描聚合 + 统一发现清单 DTO
pub use aggregate::{
    DISCOVERY_PORT, DiscoveryResp, ScannedNode, StreamView, probe_base, scan, scan_lan, to_views,
};
