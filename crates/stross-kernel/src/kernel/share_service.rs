//! v3 §3.3 共享契约实现：`impl stross_share::ShareService for Kernel`。
//!
//! 依赖方向铁律：**stross-share 只放 trait 与纯类型**，实现放内核（契约 crate
//! 不依赖 kernel）。P2e：生命周期方法体收敛进 trait 实现（`on_subscribed`
//! 登记段委托内核 `note_share_active` 核心、`reap_if_unwatched` = 旧
//! `stop_share_if_unwatched` 本体、`stop` = 旧 `stop_endpoint_share` 本体——
//! 内核内部调用一律经契约 UFCS，不再有重复自有方法）。

use std::sync::Arc;

use stross_endpoint::{ShareEndpoint, SubscribeCtx, TargetKind};
use stross_proto::message::{Delivery, EndpointId, StreamId, Visibility, derive_stream_id};
use stross_share::{ActiveShare, ShareHandle, ShareService};

use crate::Kernel;
use crate::lock::MutexExt;

impl ShareService for Kernel {
    /// 发布端点为「可被订阅」：委托 [`Kernel::publish_endpoint`]（`ep.id()` →
    /// `endpoint_id`；transports/codecs 用 `None`——注册表按端点目标类型给默认
    /// 传输档案）；返回的 [`ShareHandle`] 携带**语义流 id**（
    /// `derive(endpoint_id, transport_profile, pick_rule)` 三要素，与协商签发
    /// 同源——订阅达成即收敛到同一流，handle 是撤销/治理的操作凭据）。
    ///
    /// 前置：端点须已登记（`Kernel::seed_endpoint`）；文件端点动态登记走
    /// [`Kernel::publish_file_endpoint`]（路径载体，不经本契约）。
    fn publish(
        &self,
        ep: &dyn ShareEndpoint,
        visibility: Visibility,
    ) -> Result<ShareHandle, String> {
        let endpoint_id = ep.id();
        let manifest = self
            .publish_endpoint(
                endpoint_id,
                visibility,
                default_delivery(ep.target()),
                None,
                None,
            )
            .map_err(|e| e.to_user_string())?;
        let stream_id =
            derive_stream_id(&endpoint_id, manifest.transport_profile, manifest.pick_rule);
        Ok(ShareHandle(stream_id))
    }

    /// 撤销发布（端点保留在注册表，可再次通告）：从 handle 的语义流 id 反查
    /// 端点 id（活动共享表 → 已通告清单逆推），委托 [`Kernel::unpublish_endpoint`]
    /// （含活动共享联动停止）。
    fn unpublish(&self, handle: &ShareHandle) -> Result<(), String> {
        let endpoint_id = self
            .endpoint_id_of_stream(&handle.0)
            .ok_or_else(|| format!("发布句柄 {} 对应端点未找到（未发布或句柄过期）", handle.0))?;
        self.unpublish_endpoint_inner(endpoint_id)
            .map_err(|e| e.to_user_string())
    }

    /// 订阅达成回调：**订阅达成即登记 active_share**（[`Kernel::note_share_active`]——
    /// P2e 后为 `pub(crate)` 登记核心，本方法即其调用方；含「接入窗口」兜底
    /// 检查）+ 委托注册表 `on_subscribed` 触发端点自驱动 `share`
    /// （内核不分派类型）。`ep` 是契约传入的端点对象——注册表为行为对象单一
    /// 真源，经它按 `endpoint_id` 取同源 Arc 出锁调用（防持锁死锁）。
    fn on_subscribed(&self, ep: &dyn ShareEndpoint, ctx: &SubscribeCtx, endpoint_id: EndpointId) {
        let Some(app) = self.self_arc() else {
            tracing::warn!(
                "ShareService::on_subscribed：内核无 Arc 自引用（非 new_arc 构造？），\
                 跳过端点 {endpoint_id} 的 share 触发（订阅 {}）",
                ctx.subscriber
            );
            return;
        };
        // 订阅达成即登记（含接入窗口兜底检查；端点状态置 Active）
        app.note_share_active(
            Arc::downgrade(&app),
            endpoint_id,
            ctx.stream_id.as_str(),
            ctx.delivery,
        );
        // 委托注册表 on_subscribed：取 Arc 出锁调用 ep.share；注册表取不到
        // （契约调用方传入未登记端点对象）时直接用 `ep` 触发。
        if !self
            .registry
            .lock_poisoned()
            .on_subscribed(&app, endpoint_id, ctx)
        {
            let host: Arc<dyn stross_endpoint::ShareHost> = app.clone();
            let runtime: Arc<dyn stross_endpoint::Runtime> = app;
            ep.share(host, runtime, ctx.clone());
        }
    }

    /// 生命周期治理：watchers 归零复查仍无人观看时停止共享——**P2e 方法体
    /// 收敛**（旧自有方法 [`Kernel::stop_share_if_unwatched`] 已删除，本方法即
    /// 其本体；默认延迟 [`Kernel::share_stop_delay`] 由数据面事件转发侧控制）。
    fn reap_if_unwatched(&self, stream: &StreamId) {
        let Some(dp) = self.data_plane.lock_poisoned().clone() else {
            return;
        };
        // 流已消失（StreamEnded 路径清理）或有观众接入时不动
        if let Some(0) = dp.stream_watchers(stream.as_str()) {
            self.stop_share_by_stream(stream);
        }
    }

    /// 显式停止端点共享（保留通告；同端点订阅收敛 / 取消通告联动）——**P2e
    /// 方法体收敛**（旧自有方法 [`Kernel::stop_endpoint_share`] 已删，本方法即
    /// 其本体；幂等：无活动共享直接成功）。内核内部调用一律经本契约 UFCS。
    fn stop(&self, endpoint_id: EndpointId) -> Result<(), String> {
        let sid = self
            .active_shares
            .lock_poisoned()
            .iter()
            .find_map(|(sid, s)| (s.endpoint_id == endpoint_id).then(|| sid.clone()));
        let Some(sid) = sid else {
            return Ok(()); // 无活动共享
        };
        self.stop_share_by_stream(&sid);
        Ok(())
    }

    /// 当前活动共享快照（stream → 登记）：从 `active_shares` 表投影，订阅者
    /// 节点集经注册表 [`EndpointRegistry::subscriber_nodes`] 补全（单一真源：
    /// `note_endpoint_subscribed` 维护的显式订阅节点集）。
    fn active(&self) -> Vec<(StreamId, ActiveShare)> {
        let pairs: Vec<(StreamId, ActiveShare)> = self
            .active_shares
            .lock_poisoned()
            .iter()
            .map(|(s, a)| (s.clone(), a.clone()))
            .collect();
        let reg = self.registry.lock_poisoned();
        let mut v: Vec<(StreamId, ActiveShare)> = pairs
            .into_iter()
            .map(|(s, mut a)| {
                a.subscriber_nodes = reg.subscriber_nodes(a.endpoint_id).unwrap_or_default();
                (s, a)
            })
            .collect();
        drop(reg);
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }
}

impl Kernel {
    /// 反查发布句柄（语义流 id）对应的端点 id：先查活动共享表（流 → 登记），
    /// 再扫已通告清单逆推（语义 id 由端点三要素确定性派生，可逆推；未订阅的
    /// 发布句柄走这条）。
    fn endpoint_id_of_stream(&self, stream: &StreamId) -> Option<EndpointId> {
        if let Some(s) = self
            .active_shares
            .lock_poisoned()
            .iter()
            .find(|(sid, _)| *sid == stream)
            .map(|(_, s)| s.endpoint_id)
        {
            return Some(s);
        }
        for m in self.registry.lock_poisoned().published_manifests() {
            let id = EndpointId::new(m.kind, m.endpoint_id);
            let sid = derive_stream_id(&id, m.transport_profile, m.pick_rule);
            if &sid == stream {
                return Some(id);
            }
        }
        None
    }
}

/// 端点 → 默认数据面方向（订阅驱动定稿只走 pull，docs/framework-v3.md §3.4）：
/// 实时/确定目标一律 Pull——订阅方连公开方中继 watch 取流；保留按目标类型的
/// 分型推导（与 [`super::endpoint::EndpointRegistry::default_transports`] 同源），
/// 未来出现出站 push 能力时按目标类型扩展。
fn default_delivery(target: TargetKind) -> Delivery {
    match target {
        TargetKind::Live | TargetKind::Determined => Delivery::Pull,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MicEndpoint, Platform, Probe};
    use stross_proto::message::{EndpointStrategy, NodeId, PickRule, ReliabilityProfile};

    fn ok_probe() -> Probe {
        Arc::new(|| Ok(()))
    }

    fn mic_id() -> EndpointId {
        EndpointId::new(stross_proto::message::MediaKind::Mic, 0)
    }

    fn mic_ctx() -> SubscribeCtx {
        SubscribeCtx {
            subscriber: NodeId::from("dev-phone"),
            delivery: Delivery::Pull,
            stream_id: StreamId::from("sess-1"),
            transport_profile: ReliabilityProfile::Lossy,
            strategy: EndpointStrategy::passthrough(PickRule::Realtime),
            relay_addr: None,
            share_token: None,
        }
    }

    /// publish → manifest（已通告 + 默认 Pull）+ handle 语义流 id 可逆推。
    #[test]
    fn publish_unpublish_roundtrip() {
        let k = Kernel::new(Platform::Desktop);
        k.seed_endpoint(Box::new(MicEndpoint::new("麦克风", ok_probe())));
        let ep = k
            .registry
            .lock_poisoned()
            .endpoint_arc(mic_id())
            .expect("麦克风端点已登记");
        let handle = k
            .publish(ep.as_ref(), Visibility::Public)
            .expect("通告成功");
        let m = k.endpoint_manifest(mic_id()).expect("清单可查");
        assert!(m.published, "发布后 manifest.published = true");
        assert_eq!(m.delivery, Delivery::Pull, "订阅驱动默认 Pull");
        // handle = 语义流 id（同源三要素派生）
        assert_eq!(
            handle.0,
            derive_stream_id(&mic_id(), m.transport_profile, m.pick_rule),
            "发布句柄携带语义流 id"
        );
        // 未订阅时无活动共享
        assert!(k.active().is_empty());
        // 撤销 → manifest 恢复未通告
        k.unpublish(&handle).expect("撤销成功");
        assert!(!k.endpoint_manifest(mic_id()).unwrap().published);
        // 重复撤销：句柄已失效 → 报错
        assert!(k.unpublish(&handle).is_err());
    }

    /// 不可挂载端点（load 探测失败）拒绝发布。
    #[test]
    fn publish_rejects_unavailable_endpoint() {
        let k = Kernel::new(Platform::Desktop);
        k.seed_endpoint(Box::new(MicEndpoint::new(
            "麦克风",
            Arc::new(|| Err("无采集设备".into())),
        )));
        let ep = k
            .registry
            .lock_poisoned()
            .endpoint_arc(mic_id())
            .expect("已登记");
        let err = k
            .publish(ep.as_ref(), Visibility::Public)
            .expect_err("不可挂载端点应拒绝发布");
        assert!(err.contains("不可挂载"), "错误携带 load 探测原因: {err}");
    }

    /// 订阅达成回调：登记 active_share（含订阅者节点集投影）+ 端点 share 触发
    /// （无采集后端时 share 异步失败仅告警，不影响登记）。
    #[tokio::test]
    async fn on_subscribed_registers_active_share() {
        let k = Kernel::new_arc(Platform::Desktop);
        k.seed_endpoint(Box::new(MicEndpoint::new("麦克风", ok_probe())));
        let ep = k
            .registry
            .lock_poisoned()
            .endpoint_arc(mic_id())
            .expect("已登记");
        // 订阅者先显式登记（note_endpoint_subscribed 是订阅者集单一真源）
        k.note_endpoint_subscribed(mic_id(), NodeId::from("dev-phone"));
        let ctx = mic_ctx();
        k.on_subscribed(ep.as_ref(), &ctx, mic_id());
        let active = k.active();
        assert_eq!(active.len(), 1, "订阅达成即登记一条活动共享");
        assert_eq!(active[0].0, StreamId::from("sess-1"));
        assert_eq!(active[0].1.endpoint_id, mic_id());
        assert_eq!(active[0].1.delivery, Delivery::Pull);
        assert_eq!(
            active[0].1.subscriber_nodes,
            vec![NodeId::from("dev-phone")],
            "订阅者节点集投影自注册表"
        );
        // 显式停止 → 登记清除
        k.stop(mic_id()).expect("停止成功");
        assert!(k.active().is_empty(), "停止后登记清除");
        // 幂等：无活动共享时停止直接成功
        assert!(k.stop(mic_id()).is_ok());
    }

    /// 生命周期治理委托：reap_if_unwatched 无数据面时不动作（不 panic）。
    #[test]
    fn reap_if_unwatched_without_data_plane_is_noop() {
        let k = Kernel::new(Platform::Desktop);
        k.reap_if_unwatched(&StreamId::from("sess-x"));
        // 未接入数据面 → 直接返回（无观察者查询能力）
    }

    /// 端点 class 展示（MicEndpoint 归 Audio 能力族；契约 active 不依赖它，
    /// 顺带验证种子端点的能力族推导）。
    #[test]
    fn seeded_endpoint_class_is_audio() {
        let k = Kernel::new(Platform::Desktop);
        k.seed_endpoint(Box::new(MicEndpoint::new("麦克风", ok_probe())));
        let ep = k.registry.lock_poisoned().endpoint_arc(mic_id()).unwrap();
        assert_eq!(
            stross_endpoint::EndpointClass::from_kind(ep.kind()),
            stross_endpoint::EndpointClass::Audio
        );
    }
}
