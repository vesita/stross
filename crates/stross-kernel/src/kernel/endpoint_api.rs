//! 内核端点框架域（`impl Kernel`）：通告 / 三层注册表 / 共享生命周期。
//!
//! docs/layering-architecture.md：`Kernel` 单一门面；本文件承载「端点框架
//! （通告 / 注册表 / 订阅联动 / 共享生命周期治理）」一域的实现，
//! 方法与公共 API 不变。

use std::path::Path;
use std::sync::Arc;

use stross_endpoint::EndpointApp;
use stross_proto::message::{
    CodecId, Delivery, EndpointId, EndpointManifest, EndpointState, NodeId, SubscribeSpec,
    Visibility,
};
use stross_types::LocalCatalog;

use crate::Kernel;
use crate::error::{Error, Result};
use crate::lock::MutexExt;

use super::{ActiveShare, EndpointRegistry, Id, StreamId, SubscribeCtx};

impl Kernel {
    // -----------------------------------------------------------------------
    // 端点框架（三层端点模型：节点 → 端点 → 策略，见 docs/endpoint-model-v2.md）
    // -----------------------------------------------------------------------

    /// 通告端点为可订阅（可见性 / delivery / 传输由公开者声明）。
    ///
    /// 不可挂载端点（`available=false`）拒绝通告，错误携带 load 探测原因
    /// （如「无图形会话」——屏幕获取失败前置化）。`transports` / `codecs`
    /// 缺省时按端点**目标类型**给默认（实时目标 Lossy → QUIC>SRT>WS，
    /// 确定目标 Lossless → QUIC>WS）。
    pub fn publish_endpoint(
        &self,
        endpoint_id: EndpointId,
        visibility: Visibility,
        delivery: Delivery,
        transports: Option<Vec<stross_proto::message::TransportPreference>>,
        codecs: Option<Vec<CodecId>>,
    ) -> Result<EndpointManifest> {
        let manifest = {
            let mut reg = self.registry.lock_poisoned();
            let target = reg.target(endpoint_id).unwrap_or(super::TargetKind::Live);
            let transports =
                transports.unwrap_or_else(|| EndpointRegistry::default_transports(target));
            let codecs = codecs.unwrap_or_else(|| vec![CodecId::H264, CodecId::Aac]);
            reg.publish(endpoint_id, visibility, delivery, transports, codecs)?
        };
        // 通告 → 立即刷新 mDNS 端点摘要（可被发现时；锁外，避免 registry→anchor 反序）
        self.apply_discoverable();
        Ok(manifest)
    }

    /// 取消通告端点（端点保留在表里可再次通告；已订阅会话由上层决定宽限期）。
    ///
    /// **活动共享联动**：该端点若正在被订阅观看，先停止共享并拆除会话
    /// （取消通告 = 不再共享，踢出当前订阅者）。
    pub async fn unpublish_endpoint(&self, endpoint_id: EndpointId) -> Result<()> {
        self.stop_endpoint_share(endpoint_id)?;
        self.registry.lock_poisoned().unpublish(endpoint_id)?;
        // 取消通告 → 立即刷新 mDNS 端点摘要（锁外）
        self.apply_discoverable();
        Ok(())
    }

    /// 公开本地文件为文件端点（动态端点 `file:<名>`；本地路径登记但不出现在
    /// 目录 / 摘要 / wire，见 docs/endpoint-model-v2.md §3）。
    pub fn publish_file_endpoint(
        &self,
        path: &Path,
        visibility: Visibility,
        delivery: Delivery,
    ) -> Result<EndpointManifest> {
        let manifest = self
            .registry
            .lock_poisoned()
            .publish_file(path, visibility, delivery)?;
        // 通告 → 立即刷新 mDNS 端点摘要（锁外）
        self.apply_discoverable();
        Ok(manifest)
    }

    /// 文件端点的本地文件源（control.rs 状态展示）。
    pub fn file_source(&self, endpoint_id: EndpointId) -> Option<super::FileSource> {
        self.registry
            .lock_poisoned()
            .file_source(endpoint_id)
            .cloned()
    }

    /// 订阅达成事件（协商层授予成功后调用）：触发端点 `share`（端点自驱动，
    /// 内核不做类型分派）。
    ///
    /// share 在注册表锁**外**调用（端点实现会再次访问内核），持锁回调会死锁。
    pub fn on_endpoint_subscribed(
        &self,
        app: Arc<Self>,
        endpoint_id: EndpointId,
        ctx: &SubscribeCtx,
    ) {
        // 严格出锁调用：提取端点 Arc 后立即释放注册表锁，防止端点实现重入内核引发死锁
        let ep = self.registry.lock_poisoned().endpoint_arc(endpoint_id);
        if let Some(ep) = ep {
            ep.share(app, ctx.clone());
        }
    }

    /// 端点清单查询（订阅握手 / 目录 API 用）。
    pub fn endpoint_manifest(&self, endpoint_id: EndpointId) -> Option<EndpointManifest> {
        self.registry.lock_poisoned().manifest(endpoint_id)
    }

    /// 目录快照：全部端点清单（Private / 未通告可见性过滤由调用方做）。
    pub fn endpoint_catalog(&self) -> Vec<EndpointManifest> {
        self.registry.lock_poisoned().manifests()
    }

    /// 已通告端点清单（对端目录用；Private 过滤由协商层做）。
    pub fn published_endpoints(&self) -> Vec<EndpointManifest> {
        self.registry.lock_poisoned().published_manifests()
    }

    /// 本机目录视图（全部端点；节点卡片端点树渲染用）。
    pub fn local_catalog(&self) -> LocalCatalog {
        let endpoints = self.endpoint_catalog();
        LocalCatalog { endpoints }
    }

    // -----------------------------------------------------------------------
    // 统一注册表（v2 三层：节点 → 端点 → 策略；docs/endpoint-model-v2.md §2）
    // -----------------------------------------------------------------------

    /// 把目录响应（`GET /api/endpoints`）的互联节点映射进统一注册表
    /// （节点 → 端点 → 策略）。订阅方拉取目录后调用——与 mDNS 摘要不同，
    /// 目录携带完整策略组合（序列化 + pick）。
    pub fn register_remote_directory(&self, dir: &stross_proto::message::EndpointDir, addr: &str) {
        self.registry
            .lock_poisoned()
            .register_remote_directory(dir, addr);
    }

    /// 统一查表：`registry[节点][端点][策略]` → 策略组合。
    /// 自订（本机节点）与订其它互联节点走同一套逻辑；`strategy_id` 缺省 =
    /// 端点默认策略。
    pub fn resolve_strategy(
        &self,
        node_id: &NodeId,
        endpoint_id: EndpointId,
        strategy_id: Option<&str>,
    ) -> Option<stross_proto::message::EndpointStrategy> {
        self.registry
            .lock_poisoned()
            .resolve_strategy(node_id, endpoint_id, strategy_id)
    }

    /// 三层注册表快照（节点 → 端点 → 策略；含本机镜像；UI / 调试用）。
    pub fn registry_nodes(&self) -> Vec<super::NodeRegistration> {
        self.registry.lock_poisoned().node_registrations()
    }

    /// 订阅端点生成 + 委托（v2 订阅端，docs/endpoint-model-v2.md §3）：
    /// 从注册表 `(节点, 端点, 策略)` 生成订阅端点并调其 `subscribe`——
    /// 与分享端 `share` 同构（端点自驱动，内核不分派）。订阅目标类型暂无
    /// 订阅端点宿主时返回错误（媒体播放由接收链路承担）。
    pub fn subscribe_via_endpoint(
        &self,
        app: Arc<Self>,
        spec: &SubscribeSpec,
        out_dir: Option<&Path>,
    ) -> Result<()> {
        let ep = self
            .registry
            .lock_poisoned()
            .generate_subscribe_endpoint(spec, out_dir)
            .ok_or_else(|| {
                Error::Message(format!(
                    "端点「{}」的订阅目标类型暂无订阅端点宿主（生成订阅端点失败）",
                    EndpointId::new(spec.kind, spec.endpoint_id)
                ))
            })?;
        ep.subscribe(app, spec.clone());
        Ok(())
    }

    // -----------------------------------------------------------------------
    // 端点共享生命周期（iteration-plan.md 第十二轮）
    // -----------------------------------------------------------------------

    /// 端点共享登记（媒体端点 `start_stream` 成功后由端点层回调，
    /// 见 [`EndpointApp::note_share_active`]）：登记 + 状态置 Active +
    /// 启动"无观看者接入窗口"兜底检查（订阅者从未接入时停止）。
    pub fn note_share_active(
        &self,
        self_weak: std::sync::Weak<dyn EndpointApp>,
        endpoint_id: EndpointId,
        stream_id: &str,
        delivery: Delivery,
    ) {
        {
            let mut shares = self.active_shares.lock_poisoned();
            shares.insert(
                Id::from(stream_id),
                ActiveShare {
                    endpoint_id,
                    delivery,
                },
            );
        }
        let _ = self
            .registry
            .lock_poisoned()
            .set_state(endpoint_id, EndpointState::Active, 0);
        tracing::info!("端点共享已登记: {endpoint_id} → {stream_id} ({delivery:?})");
        // 接入窗口兜底：与事件顺序无关（StreamStarted 可能先于登记到达转发任务），
        // 因此在登记处统一启动检查（经弱引用回调，不拖住内核）。
        let stream_id = Id::from(stream_id);
        let idle = self.share_idle_delay;
        tokio::spawn(async move {
            tokio::time::sleep(idle).await;
            if let Some(app) = self_weak.upgrade() {
                app.stop_share_if_unwatched(&stream_id);
            }
        });
    }

    /// 停止指定端点的活动共享（幂等：无活动共享时直接成功）。
    pub fn stop_endpoint_share(&self, endpoint_id: EndpointId) -> Result<()> {
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

    /// 按流停止端点共享：清登记 + 复位状态 + 优雅停流 + 拆除本机会话。
    /// （同步：停流仅取出引擎并 spawn 收尾，不在本路径 await。）
    fn stop_share_by_stream(&self, stream_id: &Id) {
        if let Some(endpoint_id) = self.reap_stream(stream_id) {
            tracing::info!("端点共享停止: {endpoint_id} (stream={stream_id})");
        }
    }

    /// 流结束公共清理（`stop_share_by_stream` 与数据面 `StreamEnded` 事件共用）：
    /// ①清端点共享登记并复位状态；②按 stream_id 移除并发推流引擎并**优雅停流**
    /// （防采集进程中断后该流残留、卡住同 id 重推）；③拆除本机会话
    /// （会话生命周期 = 流生命周期；远程 push 会话不在本机 → SessionNotFound 忽略）。
    ///
    /// 返回是否清除了**端点共享登记**（调用方据此判断来源是否端点共享流）。
    /// ②③ 对非端点共享流（如远程 push、QUIC 推流）同样执行——它们的
    /// 引擎/会话清理不依赖端点登记。同步非阻塞：停流仅取出引擎并 spawn 收尾。
    pub(crate) fn reap_stream(&self, stream_id: &Id) -> Option<EndpointId> {
        // ① 先取走登记（并发到达的停止请求只执行一次），并复位端点状态；
        //    非端点共享流无登记 → endpoint_id 为 None，其余清理仍继续。
        let endpoint_id = self.clear_active_share(stream_id).map(|share| {
            let mut reg = self.registry.lock_poisoned();
            reg.clear_subscribers(share.endpoint_id);
            let _ = reg.set_state(share.endpoint_id, EndpointState::Idle, 0);
            share.endpoint_id
        });
        // ② 优雅停流：按 stream_id 从并发流表取出对应引擎，仅在存在时动作
        if let Some(stream) = self.engines.lock_poisoned().remove(stream_id) {
            tokio::spawn(async move {
                stream.engine.stop().await;
            });
        }
        // ③ 拆除本机会话
        if self.has_session(stream_id.as_str()) {
            let _ = self.force_teardown(stream_id.as_str());
        }
        endpoint_id
    }

    /// watchers 归零复查：仍无人观看才停（期间有新观众接入则放弃）。
    pub(crate) fn stop_share_if_unwatched(&self, stream_id: &Id) {
        let Some(dp) = self.data_plane.lock_poisoned().clone() else {
            return;
        };
        // 流已消失（StreamEnded 路径清理）或有观众接入时不动
        if let Some(0) = dp.stream_watchers(stream_id.as_str()) {
            self.stop_share_by_stream(stream_id);
        }
    }

    /// 生命周期治理延迟（默认 stop 4s / idle 10s；测试与嵌入式调用方可按需收紧）。
    pub fn set_share_lifecycle_delays(
        &mut self,
        stop_delay: std::time::Duration,
        idle_delay: std::time::Duration,
    ) {
        self.share_stop_delay = stop_delay;
        self.share_idle_delay = idle_delay;
    }

    /// 订阅达成：记录订阅者到端点（`subscribers` 即时 +1；早于数据面
    /// watchers 事件，共享端 UI「N 订阅中」即刻反映）。协商层授成功后调用。
    pub fn note_endpoint_subscribed(&self, endpoint_id: EndpointId, node_id: NodeId) {
        self.registry
            .lock_poisoned()
            .note_subscriber(endpoint_id, node_id);
    }

    /// 订阅终止（显式取消订阅通知）：从端点移除该订阅者并更新计数；
    /// 返回移除后剩余的订阅者数（0 = 最后一个订阅者离开）。
    ///
    /// `remaining == 0` 时立即停止该端点共享（不再等数据面 watchers 断连
    /// 的延迟复查）——共享端端点状态在订阅终止瞬间收敛到已共享/待连接。
    pub fn note_endpoint_unsubscribed(&self, endpoint_id: EndpointId, node_id: NodeId) -> u32 {
        let remaining = self
            .registry
            .lock_poisoned()
            .note_unsubscriber(endpoint_id, node_id);
        if remaining == 0 {
            let _ = self.stop_endpoint_share(endpoint_id);
        }
        remaining
    }

    /// 查询端点当前活动共享（`(stream_id, delivery)`；订阅收敛用）。
    pub fn active_share_by_endpoint(
        &self,
        endpoint_id: EndpointId,
    ) -> Option<(StreamId, Delivery)> {
        self.active_shares
            .lock_poisoned()
            .iter()
            .find_map(|(sid, s)| (s.endpoint_id == endpoint_id).then(|| (sid.clone(), s.delivery)))
    }

    /// 查询流的活动共享登记（watchers 事件反查端点用）。
    pub(crate) fn active_share_by_stream(&self, stream_id: &Id) -> Option<ActiveShare> {
        self.active_shares.lock_poisoned().get(stream_id).cloned()
    }

    /// 取走流的活动共享登记（停止 / 流结束时调用）。
    pub(crate) fn clear_active_share(&self, stream_id: &Id) -> Option<ActiveShare> {
        self.active_shares.lock_poisoned().remove(stream_id)
    }
}
