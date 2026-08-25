//! `stross push`：推流。内嵌中继（`--port`）或推往外部中继（`--relay`）。
//!
//! 无设备环境用合成源（testsrc2 画面 + 可选 sine 音频）。

use std::sync::Arc;
use std::time::Duration;

use clap::Args;
use stross_app::SenderEngine;
use stross_media::capture::FfmpegBackend;
use stross_media::pipeline::{AudioSourceConfig, Quality, StreamConfig, VideoSource};

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum QualityArg {
    Low,
    Medium,
    High,
}

impl QualityArg {
    pub(crate) fn quality(self) -> Quality {
        match self {
            QualityArg::Low => Quality::LOW,
            QualityArg::Medium => Quality::MEDIUM,
            QualityArg::High => Quality::HIGH,
        }
    }
}

#[derive(Args, Debug)]
pub struct PushArgs {
    /// 推流时长（秒）
    #[arg(long, default_value_t = 15)]
    pub secs: u64,
    /// 加正弦波音频（无麦克风环境）
    #[arg(long)]
    pub audio: bool,
    /// 内嵌中继端口（0 = 随机；设了 --relay 时忽略）
    #[arg(long, default_value_t = 0)]
    pub port: u16,
    /// 推往外部中继（ws://host:port/ws/push / srt://host:port / quic://host:port，
    /// 按 scheme 选传输；设了则不再启动内嵌中继）
    #[arg(long)]
    pub relay: Option<String>,
    /// 流 id（默认 demo-<pid>）
    #[arg(long)]
    pub stream_id: Option<String>,
    /// 画质
    #[arg(long, value_enum, default_value_t = QualityArg::Medium)]
    pub quality: QualityArg,
}

pub async fn run(args: PushArgs) -> anyhow::Result<()> {
    let stream_id = args
        .stream_id
        .unwrap_or_else(|| format!("demo-{}", std::process::id()));
    let mut cfg = StreamConfig {
        stream_id: stream_id.clone(),
        title: "CLI 推流".into(),
        video: Some(VideoSource::Synthetic {
            pattern: "testsrc2".into(),
        }),
        quality: args.quality.quality(),
        audio: None,
        duration_secs: Some(args.secs as u32),
    };
    if args.audio {
        cfg.audio = Some(AudioSourceConfig::default());
    }

    let engine = match SenderEngine::start(
        cfg.clone(),
        Arc::new(FfmpegBackend::new()),
        args.relay.clone(),
        args.port,
    )
    .await
    {
        Ok(e) => e,
        Err(_) if args.audio => {
            tracing::warn!("音频启动失败，退回纯视频");
            cfg.audio = None;
            SenderEngine::start(cfg, Arc::new(FfmpegBackend::new()), args.relay.clone(), args.port).await?
        }
        Err(e) => return Err(e),
    };

    match engine.relay_port() {
        Some(port) => {
            tracing::info!(
                "📡 推流中（{} 秒）: {stream_id} @ 内嵌中继 ws://<本机IP>:{port}（自动选传输）",
                args.secs
            );
        }
        None => {
            let url = args.relay.as_deref().unwrap_or("<自动>");
            tracing::info!("📡 推流中（{} 秒）: {stream_id} → {url}", args.secs);
        }
    }
    tokio::time::sleep(Duration::from_secs(args.secs)).await;
    engine.stop().await;
    tracing::info!("推流结束: {stream_id}");
    Ok(())
}
