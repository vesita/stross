//! v3 §3.4 订阅契约实现：`impl stross_subscribe::SubscribeService for Kernel`。
//!
//! 依赖方向铁律：**stross-subscribe 只放 trait 与纯类型**，实现放内核（契约
//! crate 不依赖 kernel）。本文件是 kernel `subscriber` / `receiver` /
//! `generate_subscribe_endpoint` 编排的**契约接入面**（委托不重写）——
//! 解析走注册表三层查表，建链把订阅端点交给运行时（端点自驱动，与分享端
//! `share` 同构），链路运行态仍走既有接收编排（壳层路径保留）。

use std::str::FromStr;
use std::sync::Arc;

use stross_endpoint::{Runtime, SubscribeEndpoint, SubscribeHost};
use stross_proto::message::{
    Delivery, EndpointId, LinkId, MediaKind, NodeId, ReliabilityProfile, StrategyId, StreamId,
    SubscribeSpec, derive_stream_id,
};
use stross_subscribe::{SubscribeLink, SubscribeService};

use crate::Kernel;
use crate::lock::MutexExt;

use super::{Id, ReceiveLinkMeta};

impl SubscribeService for Kernel {
    /// 解析 `(节点, 端点, 策略)` → 订阅规格（注册表统一查表，委托
    /// [`Kernel::resolve_strategy`]；未知返回 `None`）。
    ///
    /// * 语义流 id：与共享方协商签发**同源三要素**派生
    ///   （`derive(endpoint, transport_profile, pick)`——订阅方本地可推导，
    ///   watch 用同一 id）；
    /// * `relay_url`：本机端点 → 本机中继 WS 基址（锚定后）；互联节点注册表
    ///   不携带中继端口（目录不含），由调用方经握手授予补全。
    fn resolve(
        &self,
        node: &NodeId,
        endpoint: EndpointId,
        strategy: Option<&StrategyId>,
    ) -> Option<SubscribeSpec> {
        let requested = strategy.copied();
        let strategy = self.resolve_strategy(node, endpoint, strategy.map(StrategyId::as_str))?;
        let reg = self.registry.lock_poisoned();
        let (profile, pick) = reg
            .stream_profile(node, endpoint)
            .unwrap_or((ReliabilityProfile::Lossy, strategy.pick));
        let delivery = reg
            .manifest_for(node, endpoint)
            .map(|m| m.delivery)
            .unwrap_or(Delivery::Pull);
        let is_self = *node == reg.self_node_id();
        drop(reg);
        let stream_id = derive_stream_id(&endpoint, profile, pick);
        let relay_url = if is_self {
            self.local_proxy().map(|p| p.ws_base)
        } else {
            None
        };
        Some(SubscribeSpec {
            node_id: *node,
            kind: endpoint.kind,
            endpoint_id: endpoint.id,
            strategy_id: requested,
            strategy,
            delivery,
            stream_id,
            relay_url,
        })
    }

    /// 建链：分配链路槽位（[`LinkId`] 数值单调，`0` = 预留 `main` 槽）+ 登记
    /// 链路元数据（`links()` 投影的节点/端点/流身份）+ 把订阅端点交给运行时
    /// （端点自驱动 `receive_media` / `receive_file`，内核不分派类型）。
    ///
    /// **命名注意**：本契约方法与内核固有方法 [`Kernel::subscribe`]（内核事件
    /// 订阅）同名——固有方法遮蔽 trait 方法，调用方须经 UFCS
    /// `<Kernel as SubscribeService>::subscribe(…)` 或 `Arc<dyn SubscribeService>`
    /// 走契约路径。
    ///
    /// **简化（链路模型适配）**：实际媒体接收仍走既有接收编排
    /// （[`Kernel::start_receive_link`] 壳层路径保留）——本方法先做
    /// 「登记 + 调用 sink」，返回分配的 `LinkId`；订阅端点自驱动的接收会话
    /// （独立 `Receiver`）后续并入 receivers 表（P3 迁移）。
    fn subscribe(
        &self,
        spec: SubscribeSpec,
        sink: Box<dyn SubscribeEndpoint>,
    ) -> Result<LinkId, String> {
        // 分配链路槽位（0 = main 预留槽，契约链路从 1 起）
        let link_id = LinkId::new(
            self.next_link_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed) as u32
                + 1,
        );
        // 登记链路元数据（links() 投影用；运行态由订阅端点自驱动）
        self.receive_link_meta.lock_poisoned().insert(
            Id::from(link_id.to_string()),
            ReceiveLinkMeta {
                node_id: spec.node_id,
                endpoint_id: Some(EndpointId::new(spec.kind, spec.endpoint_id)),
                stream_id: Some(spec.stream_id.clone()),
            },
        );
        // 建链：host = 订阅端可见能力组合（MediaHost + FileHost），runtime =
        // fire-and-forget 载体；端点自驱动，内核不分派。
        let app = self.self_arc().ok_or_else(|| {
            "订阅建链失败：内核无 Arc 自引用（非 new_arc 构造？无法构造订阅能力对象）".to_string()
        })?;
        let host: Arc<dyn SubscribeHost> = app.clone();
        let runtime: Arc<dyn Runtime> = app;
        sink.subscribe(host, runtime, spec);
        Ok(link_id)
    }

    /// 停止一条订阅链路（多链路互不级联）：委托 [`Kernel::stop_receive_link`]
    /// （契约链路 id → receivers 键形态 `"main"` / `"link-N"`；无该链路时
    /// 静默成功，幂等）。
    fn unsubscribe(&self, link: &LinkId) -> Result<(), String> {
        self.stop_receive_link(&link.to_string());
        Ok(())
    }

    /// 全部链路快照：从 receivers 表投影（运行态 + 统计），节点/端点/流身份
    /// 经 [`ReceiveLinkMeta`] 补全（契约 `subscribe` 与壳层 `start_receive_link`
    /// 登记；未知字段回落占位——P3 迁移把契约链路并入 receivers 表）。
    fn links(&self) -> Vec<SubscribeLink> {
        let guard = self.receivers.lock_poisoned();
        let mut v = Vec::with_capacity(guard.len());
        for (id, r) in guard.iter() {
            let stats = r.stats();
            let meta = self
                .receive_link_meta
                .lock_poisoned()
                .get(id)
                .cloned()
                .unwrap_or_default();
            v.push(SubscribeLink {
                link_id: LinkId::from_str(id.as_str()).unwrap_or(LinkId::MAIN),
                node_id: meta.node_id,
                endpoint_id: meta.endpoint_id.unwrap_or_else(|| {
                    // 未知端点的占位（旧链路未登记元数据；无未知变体，取 Service 族）
                    EndpointId::new(MediaKind::Service, 0)
                }),
                stream_id: meta
                    .stream_id
                    .clone()
                    .unwrap_or_else(|| StreamId::from(id.as_str())),
                running: stats.running,
            });
        }
        drop(guard);
        v.sort_by_key(|l| l.link_id);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MicEndpoint, Platform, Probe};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use stross_endpoint::contract::{Endpoint, EndpointBase, TargetKind};
    use stross_proto::message::{EndpointStrategy, PickRule, ReliabilityProfile, Visibility};

    fn ok_probe() -> Probe {
        Arc::new(|| Ok(()))
    }

    fn mic_id() -> EndpointId {
        EndpointId::new(MediaKind::Mic, 0)
    }

    fn spec_for_mic() -> SubscribeSpec {
        SubscribeSpec {
            node_id: NodeId::from("node-phone"),
            kind: MediaKind::Mic,
            endpoint_id: 0,
            strategy_id: None,
            strategy: EndpointStrategy::passthrough(PickRule::Realtime),
            delivery: Delivery::Pull,
            stream_id: StreamId::from("sess-1"),
            relay_url: Some("ws://192.168.1.5:18777".into()),
        }
    }

    /// 本机端点 resolve → 完整 SubscribeSpec（策略查表 + 语义流 id 与共享侧
    /// 派生一致 + 本机 relay 基址）。
    #[test]
    fn resolve_builds_subscribe_spec_for_self() {
        let k = Kernel::new(Platform::Desktop);
        k.set_identity(crate::negotiator::NodeIdentity {
            node_id: NodeId::from("node-pc"),
            node_name: "电脑".into(),
        });
        k.seed_endpoint(Box::new(MicEndpoint::new("麦克风", ok_probe())));
        let spec = k
            .resolve(&NodeId::from("node-pc"), mic_id(), None)
            .expect("本机麦克风端点应可解析");
        assert_eq!(spec.kind, MediaKind::Mic);
        assert_eq!(spec.endpoint_id, 0);
        assert_eq!(spec.node_id, NodeId::from("node-pc"));
        assert_eq!(spec.delivery, Delivery::Pull);
        assert_eq!(spec.strategy.pick, PickRule::Realtime);
        assert_eq!(spec.strategy_id, None, "缺省 = 端点默认策略");
        // 语义流 id 与共享侧派生同源（manifest 三要素）
        let m = k.endpoint_manifest(mic_id()).unwrap();
        assert_eq!(
            spec.stream_id,
            derive_stream_id(&mic_id(), m.transport_profile, m.pick_rule),
            "resolve 的语义流 id 与共享侧签发一致"
        );
        // 未锚定中继 → relay_url None（调用方从握手授予补全）
        assert!(spec.relay_url.is_none());
        // 显式策略 id
        let spec2 = k
            .resolve(
                &NodeId::from("node-pc"),
                mic_id(),
                Some(&StrategyId::Default),
            )
            .expect("显式默认策略可解析");
        assert_eq!(spec2.strategy_id, Some(StrategyId::Default));
        // 未知端点 → None
        assert!(
            k.resolve(
                &NodeId::from("node-pc"),
                EndpointId::new(MediaKind::Service, 99),
                None
            )
            .is_none()
        );
    }

    /// 互联节点 resolve：目录映射后按 `(节点, 端点, 策略)` 查表组装规格。
    #[test]
    fn resolve_builds_subscribe_spec_for_remote() {
        let k = Kernel::new(Platform::Desktop);
        k.set_identity(crate::negotiator::NodeIdentity {
            node_id: NodeId::from("node-pc"),
            node_name: "电脑".into(),
        });
        // 目录（远端屏幕 + 文件端点）→ 统一注册表映射
        let dir = stross_proto::message::EndpointDir {
            node: stross_proto::message::EndpointNode {
                node_id: NodeId::from("node-phone"),
                node_name: "手机A".into(),
            },
            endpoints: vec![stross_proto::message::EndpointManifest {
                endpoint_id: 0,
                kind: MediaKind::Screen,
                name: "屏幕".into(),
                available: true,
                last_error: None,
                published: true,
                visibility: Visibility::Public,
                delivery: Delivery::Pull,
                transports: vec![],
                transport_profile: ReliabilityProfile::Lossy,
                pick_rule: PickRule::Realtime,
                strategies: vec![EndpointStrategy::passthrough(PickRule::Realtime)],
                codecs: vec![],
                state: stross_proto::message::EndpointState::Idle,
                subscribers: 0,
                updated_at: 0,
            }],
        };
        k.register_remote_directory(&dir, "192.168.1.5:18779");
        let spec = k
            .resolve(
                &NodeId::from("node-phone"),
                EndpointId::new(MediaKind::Screen, 0),
                None,
            )
            .expect("远端屏幕端点应可解析");
        assert_eq!(spec.kind, MediaKind::Screen);
        assert_eq!(spec.node_id, NodeId::from("node-phone"));
        // 远端中继端口不在注册表 → relay_url None（握手授予补全）
        assert!(spec.relay_url.is_none());
    }

    /// 契约建链：分配 LinkId + 调用订阅端点（sink 自驱动）+ 幂等注销。
    #[tokio::test]
    async fn subscribe_allocates_link_and_invokes_sink() {
        let k = Kernel::new_arc(Platform::Desktop);
        let fired = Arc::new(AtomicUsize::new(0));
        let f = fired.clone();
        struct CountingSink {
            base: EndpointBase,
            fired: Arc<AtomicUsize>,
        }
        impl Endpoint for CountingSink {
            fn id(&self) -> EndpointId {
                self.base.id
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
            fn transport_profile(&self) -> ReliabilityProfile {
                ReliabilityProfile::Lossy
            }
            fn strategy(&self) -> EndpointStrategy {
                EndpointStrategy::passthrough(PickRule::Realtime)
            }
        }
        impl SubscribeEndpoint for CountingSink {
            fn subscribe(
                &self,
                _host: Arc<dyn stross_endpoint::SubscribeHost>,
                _runtime: Arc<dyn stross_endpoint::Runtime>,
                spec: SubscribeSpec,
            ) {
                assert_eq!(
                    spec.stream_id,
                    StreamId::from("sess-1"),
                    "sink 收到完整规格"
                );
                self.fired.fetch_add(1, Ordering::SeqCst);
            }
        }
        // 注意：契约方法 `subscribe` 与内核固有方法 `Kernel::subscribe`（内核事件
        // 订阅）同名——固有方法遮蔽 trait 方法，契约方法须经 UFCS 调用
        // （壳层 P3 迁移经 `Arc<dyn SubscribeService>` 或 UFCS）。
        let link = <Kernel as stross_subscribe::SubscribeService>::subscribe(
            &k,
            spec_for_mic(),
            Box::new(CountingSink {
                base: EndpointBase {
                    id: mic_id(),
                    kind: MediaKind::Mic,
                    name: "麦克风".into(),
                    available: true,
                    last_error: None,
                },
                fired: f,
            }),
        )
        .expect("建链成功");
        assert_eq!(link, LinkId::new(1), "契约链路从 1 起（0 = main 预留槽）");
        assert_eq!(fired.load(Ordering::SeqCst), 1, "订阅端点被调用一次");
        // 契约链路运行态由 sink 自驱动（尚未进 receivers 表）→ links() 空；
        // 注销幂等
        assert!(k.links().is_empty());
        k.unsubscribe(&link).expect("注销幂等成功");
    }
}
