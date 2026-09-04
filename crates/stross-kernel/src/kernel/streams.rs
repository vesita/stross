//! 内核推流域（`impl Kernel`）：推流引擎 / 采集状态 / 并发流管理。
//!
//! docs/framework-v3.md：`Kernel` 单一门面；本文件承载「推流」
//! 一域的实现，方法与公共 API 不变。

use std::sync::atomic::Ordering;

use stross_endpoint::pipeline::StreamConfig;
use stross_proto::message::StreamId;
use stross_view::{CaptureStatusView, StartResult, StreamStatus};

use crate::engine::SenderEngine;
use crate::error::Result;
use crate::lock::MutexExt;
use crate::relay::DEFAULT_PORT;
use crate::view;

use super::{Kernel, RunningStream, SessionPrefs};

impl Kernel {
    // -----------------------------------------------------------------------
    // 推流
    // -----------------------------------------------------------------------

    /// 开始推流。
    ///
    /// * `cfg`：采集配置（视频源 / 画质 / 音频）
    /// * `relay_url`：`Some` 推到指定中继（ws:///srt:///quic://，按 scheme 选传输）；
    ///   `None` 推到常驻本机中继，地址按流媒体类型自动选传输
    ///
    /// 已接入数据面（本机受控中继）时，若 `cfg.stream_id` 还不是内核会话
    /// （旧 UI 直接推流的兜底），自动创建本机会话并由内核签发 id（D4）；
    /// 新 UI 应先 `create_session` 再传对应 id。
    pub async fn start_stream(
        &self,
        mut cfg: StreamConfig,
        relay_url: Option<String>,
    ) -> Result<StartResult> {
        // 并发推流：端点模型允许同一节点同时推多路流（屏幕 + 系统声音等），
        // 不再有「已经在推流中」的单流限制；仅同一 stream_id 重复启动则拒绝。
        let backend = self
            .backend
            .lock_poisoned()
            .clone()
            .ok_or_else(|| crate::error::Error::Message("采集后端未初始化".into()))?;
        // 会话兜底：受控中继只接受内核会话 id；未建会话时自动创建
        self.ensure_session(&mut cfg)?;
        // 未指定中继时，推到已连接（常驻）的本机中继；
        // 推流地址按媒体类型自动选传输（视频→SRT>QUIC>WS，纯音频→QUIC>WS）
        let relay_url = if let Some(u) = relay_url {
            Some(u)
        } else {
            let guard = self.anchor.lock_poisoned();
            guard
                .as_ref()
                .map(|a| a.handle.auto_push_url(cfg.video.is_some()))
        };
        let engine =
            SenderEngine::start(cfg.clone(), backend, relay_url.clone(), DEFAULT_PORT).await?;
        // 有效中继端口：内嵌中继 > 常驻中继 > 默认端口
        let relay_port = engine
            .relay_port()
            .or_else(|| self.anchor.lock_poisoned().as_ref().map(|a| a.port))
            .unwrap_or(DEFAULT_PORT);
        let started_at = stross_proto::time::unix_secs();
        {
            let sid = cfg.stream_id.clone();
            let mut g = self.engines.lock_poisoned();
            if g.contains_key(&sid) {
                return Err(crate::error::Error::Message("该流已在推流中".into()));
            }
            g.insert(
                sid,
                RunningStream {
                    engine,
                    relay_port,
                    title: cfg.title.clone(),
                    stream_id: cfg.stream_id.clone(),
                    started_at,
                },
            );
            tracing::info!("推流开始: {} (并发推流数={})", cfg.stream_id, g.len());
        }
        Ok(StartResult {
            relay_port,
            watch_urls: view::watch_urls(relay_url.as_deref(), relay_port),
            stream_id: cfg.stream_id.clone(),
        })
    }

    /// 确保 `cfg.stream_id` 是内核已签发会话（受控中继只接受内核会话 id，
    /// 需求 F2.2 / D4：id 与 stream_id 合一）。
    ///
    /// 新 UI 应先 `create_session` 取回内核签发的 id 再推流；旧 UI 直接传
    /// 自定义 id 时，在此兜底自动创建本机会话并改写 `cfg.stream_id`。
    ///
    /// **凭证推流（B1/B2）特例**：出示 `share_token` 推往远程接收端受控中继
    /// 时，`stream_id` 必须是接收端签发的会话 id——本机内核无此会话，兜底
    /// 改写会把 id 换成新会话，接收端将收不到流。因此凭证推流一律跳过。
    fn ensure_session(&self, cfg: &mut StreamConfig) -> Result<()> {
        if cfg.share_token.is_some() {
            return Ok(());
        }
        if !self.has_data_plane() || self.has_session(&cfg.stream_id) {
            return Ok(());
        }
        tracing::info!(
            "stream_id {} 未关联内核会话，自动创建本机会话",
            cfg.stream_id
        );
        // v3 P3 方法面收敛：推流域直连共享构建核心（id 签发 + build_session，
        // 与旧 `Kernel::create_session` 语义一致——受控中继只接受内核会话 id）。
        let id = StreamId::new(format!(
            "sess-{:x}",
            self.next_id.fetch_add(1, Ordering::Relaxed)
        ));
        let session = self.build_session(
            id,
            "local",
            &["local".into()],
            &SessionPrefs {
                title: cfg.title.clone(),
                ..Default::default()
            },
        )?;
        cfg.stream_id = session.id;
        Ok(())
    }

    /// 停止全部推流（CLI/控制面「停止推流」语义）。逐一取出引擎优雅停流。
    pub async fn stop_stream(&self) -> Result<()> {
        let streams: Vec<RunningStream> = {
            let mut g = self.engines.lock_poisoned();
            g.drain().map(|(_, s)| s).collect()
        };
        for stream in streams {
            tokio::spawn(async move {
                stream.engine.stop().await;
            });
        }
        Ok(())
    }

    /// 推流状态（并发流时报告第一条流的运行态；CLI/控制面为单流语义）。
    pub fn stream_status(&self) -> StreamStatus {
        let guard = self.engines.lock_poisoned();
        match guard.values().next() {
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
    /// 并发流时报告第一条流的采集态。
    pub fn capture_status(&self) -> CaptureStatusView {
        let guard = self.engines.lock_poisoned();
        let active = !guard.is_empty();
        let (started, error) = match guard.values().next() {
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

    /// 运行中推流的中继端口（供"打开观看端"使用；并发流时取第一条流）。
    pub fn stream_relay_port(&self) -> u16 {
        self.engines
            .lock_poisoned()
            .values()
            .next()
            .map_or(DEFAULT_PORT, |s| s.relay_port)
    }
}
