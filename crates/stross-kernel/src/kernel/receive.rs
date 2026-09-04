//! 内核接收编排域（`impl Kernel`）：多链路接收 + 旧 `main` 槽兼容。
//!
//! docs/framework-v3.md：`Kernel` 单一门面；本文件承载「接收播放」
//! 一域的实现，方法与公共 API 不变。

use std::sync::Arc;

#[cfg(not(target_os = "android"))]
use stross_endpoint::playback::AudioOut;
use stross_endpoint::playback::RenderedFrame;
use stross_proto::frame::Frame;

use crate::error::Result;
use crate::lock::MutexExt;
use crate::receiver::{LocalProxy, MAIN_RECEIVE_LINK, Receiver};

use super::{Id, Kernel, StreamId};

impl Kernel {
    // -----------------------------------------------------------------------
    // 接收播放（1e）
    // -----------------------------------------------------------------------

    /// 开始接收 `relay_url` 上的 `stream_id`（WS watch → 抖动缓冲 → 原生解码）。
    ///
    /// 返回的 [`Receiver`] 解码帧通道经 [`Kernel::take_receive_frames`]
    /// 交给上层（GUI 绘制）；同时只允许一个接收会话。`audio_out` 决定音频去向
    /// （设备播放 / 丢弃）。Android 请用 [`Kernel::start_receive_raw`]
    /// （编码帧 → Kotlin MediaCodec）。
    ///
    /// **旧单流兼容**：落到预留槽 [`MAIN_RECEIVE_LINK`]（启新链前先停旧链）；
    /// 多端点并发接收请用 [`Kernel::start_receive_link`]。
    #[cfg(not(target_os = "android"))]
    pub async fn start_receive(
        &self,
        relay_url: String,
        stream_id: StreamId,
        audio_out: AudioOut,
    ) -> Result<Arc<Receiver>> {
        self.stop_receive_link(MAIN_RECEIVE_LINK);
        self.start_receive_link(
            MAIN_RECEIVE_LINK.to_string(),
            relay_url,
            stream_id.to_string(),
            audio_out,
        )
        .await
    }

    /// 开始接收 `relay_url` 上的 `stream_id`（WS watch → 抖动缓冲 → **不解码**）。
    ///
    /// 编码帧经 [`Kernel::take_receive_raw_frames`] 交给上层（Android 播放：
    /// Kotlin MediaCodec 解码）；同时只允许一个接收会话。
    pub async fn start_receive_raw(
        &self,
        relay_url: String,
        stream_id: StreamId,
    ) -> Result<Arc<Receiver>> {
        self.stop_receive_link(MAIN_RECEIVE_LINK);
        self.start_receive_raw_link(
            MAIN_RECEIVE_LINK.to_string(),
            relay_url,
            stream_id.to_string(),
        )
        .await
    }

    /// 停止接收（旧单流兼容：只停预留槽 `main`，不影响其它链路）。
    pub fn stop_receive(&self) {
        self.stop_receive_link(MAIN_RECEIVE_LINK);
    }

    /// 取出当前接收会话的解码帧通道（每会话一次；`main` 槽）。
    pub fn take_receive_frames(&self) -> Option<tokio::sync::mpsc::Receiver<RenderedFrame>> {
        self.take_receive_frames_for(MAIN_RECEIVE_LINK)
    }

    /// 取出当前接收会话的编码帧通道（`start_receive_raw`；每会话一次；`main` 槽）。
    pub fn take_receive_raw_frames(&self) -> Option<tokio::sync::mpsc::Receiver<Frame>> {
        self.take_receive_raw_frames_for(MAIN_RECEIVE_LINK)
    }

    /// 取出链路 `link_id` 的解码帧通道（每会话一次；多端点链接 GUI 播放路径用）。
    pub fn take_receive_frames_for(
        &self,
        link_id: &str,
    ) -> Option<tokio::sync::mpsc::Receiver<RenderedFrame>> {
        self.receivers
            .lock_poisoned()
            .get(&Id::from(link_id))
            .and_then(|r| r.take_frames())
    }

    /// 取出链路 `link_id` 的编码帧通道（每会话一次；Android 播放路径用）。
    pub fn take_receive_raw_frames_for(
        &self,
        link_id: &str,
    ) -> Option<tokio::sync::mpsc::Receiver<Frame>> {
        self.receivers
            .lock_poisoned()
            .get(&Id::from(link_id))
            .and_then(|r| r.take_raw_frames())
    }

    /// 当前接收统计（`main` 槽；旧单流兼容）。
    pub fn receive_status(&self) -> crate::receiver::ReceiveStats {
        self.receivers
            .lock_poisoned()
            .get(&Id::from(MAIN_RECEIVE_LINK))
            .map(|r| r.stats())
            .unwrap_or_default()
    }

    /// Android 播放路径回写：Kotlin `PlaybackPlugin` 每解码一帧回调一次（`main` 槽）。
    pub fn note_android_decoded_frame(&self) {
        if let Some(r) = self
            .receivers
            .lock_poisoned()
            .get(&Id::from(MAIN_RECEIVE_LINK))
        {
            r.note_decoded_video();
        }
    }

    /// Android 播放路径回写（多端点链接路由）：把解码统计记到指定链路 `link_id`；
    /// 空字符串回落 `main` 槽（旧单流兼容）。
    pub fn note_android_decoded_frame_on(&self, link_id: &str) {
        let id = if link_id.is_empty() {
            MAIN_RECEIVE_LINK
        } else {
            link_id
        };
        if let Some(r) = self.receivers.lock_poisoned().get(&Id::from(id)) {
            r.note_decoded_video();
        }
    }

    // -----------------------------------------------------------------------
    // 多端点链接接收（通信模式 v2 Phase C「接收端多流化」）
    // -----------------------------------------------------------------------

    /// 开始接收 `relay_url` 上的 `stream_id`，登记为链路 `link_id`（多端点
    /// 链接：同进程可同时接收多条流，如屏幕 + 系统声音同播）。
    ///
    /// * 每条链独立启停 / 统计（[`Kernel::stop_receive_link`] /
    ///   [`Kernel::receive_links`]），停一条不级联其它链；
    /// * 同 `link_id` 重复启动 = 重启该链（先停旧链，幂等）；
    /// * 旧单流 API 的预留槽 `main` 也经本函数实现（兼容语义见
    ///   [`Kernel::start_receive`]）。
    ///
    /// `audio_out` 决定音频去向（设备播放 / 丢弃）。Android 请用
    /// [`Kernel::start_receive_raw_link`]（编码帧 → Kotlin MediaCodec）。
    #[cfg(not(target_os = "android"))]
    pub async fn start_receive_link(
        &self,
        link_id: String,
        relay_url: String,
        stream_id: String,
        audio_out: AudioOut,
    ) -> Result<Arc<Receiver>> {
        self.stop_receive_link(&Id::from(link_id.as_str()));
        let r =
            Receiver::start(relay_url, stream_id.clone(), audio_out, self.local_proxy()).await?;
        let id = Id::from(link_id);
        self.receivers.lock_poisoned().insert(id.clone(), r.clone());
        self.record_receive_link_meta(&id, stream_id);
        Ok(r)
    }

    /// 开始接收 `relay_url` 上的 `stream_id`（**不解码**：编码帧经
    /// [`Kernel::take_receive_raw_frames`] 交给上层），登记为链路 `link_id`。
    pub async fn start_receive_raw_link(
        &self,
        link_id: String,
        relay_url: String,
        stream_id: String,
    ) -> Result<Arc<Receiver>> {
        self.stop_receive_link(&Id::from(link_id.as_str()));
        let r = Receiver::start_raw(relay_url, stream_id.clone(), self.local_proxy()).await?;
        let id = Id::from(link_id);
        self.receivers.lock_poisoned().insert(id.clone(), r.clone());
        self.record_receive_link_meta(&id, stream_id);
        Ok(r)
    }

    /// 停止指定链路的接收（其它链路不受影响；不存在时静默成功）。
    pub fn stop_receive_link(&self, link_id: &str) {
        let id = Id::from(link_id);
        if let Some(r) = self.receivers.lock_poisoned().remove(&id) {
            r.stop();
        }
        self.receive_link_meta.lock_poisoned().remove(&id);
    }

    /// 记录接收链路元数据（[`SubscribeService::links`] 投影补充）：壳层链路只
    /// 登记流 id（节点/端点未知——契约 `subscribe` 登记完整三元组）。
    fn record_receive_link_meta(&self, id: &Id, stream_id: String) {
        self.receive_link_meta.lock_poisoned().insert(
            id.clone(),
            super::ReceiveLinkMeta {
                node_id: stross_proto::message::NodeId::NIL,
                endpoint_id: None,
                stream_id: Some(StreamId::from(stream_id)),
            },
        );
    }

    /// 全部接收链路快照（link_id + 统计；GUI 面板逐条展示）。
    pub fn receive_links(&self) -> Vec<crate::receiver::ReceiveLinkView> {
        let guard = self.receivers.lock_poisoned();
        let mut v: Vec<_> = guard
            .iter()
            .map(|(link_id, r)| crate::receiver::ReceiveLinkView {
                link_id: link_id.to_string(),
                stats: r.stats(),
            })
            .collect();
        drop(guard);
        v.sort_by(|a, b| a.link_id.cmp(&b.link_id));
        v
    }

    /// 本机中继的代理能力（观看直连失败时级联兜底）；本机中继未启动时为 `None`。
    pub(crate) fn local_proxy(&self) -> Option<LocalProxy> {
        self.anchor.lock_poisoned().as_ref().map(|a| LocalProxy {
            state: a.handle.state(),
            ws_base: crate::transport::RelayUrl::ws("127.0.0.1", a.port, None).to_string(),
        })
    }
}
