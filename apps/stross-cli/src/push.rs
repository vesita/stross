//! `stross push`：推流。内嵌中继（`--port`）或推往外部中继（`--relay`）。
//!
//! 无设备环境用合成源（testsrc2 画面 + 可选 sine 音频）。

use std::sync::Arc;
use std::time::Duration;

use clap::Args;
use stross_endpoint::capture::FfmpegBackend;
use stross_endpoint::pipeline::{AudioSourceConfig, Quality, StreamConfig, VideoSource};
use stross_kernel::SenderEngine;
use stross_proto::StreamId;

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum QualityArg {
    Low,
    Medium,
    High,
}

impl QualityArg {
    pub(crate) const fn quality(self) -> Quality {
        match self {
            Self::Low => Quality::LOW,
            Self::Medium => Quality::MEDIUM,
            Self::High => Quality::HIGH,
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
    /// 推真实屏幕（x11grab/gdigrab）而非合成画面（testsrc2）
    #[arg(long)]
    pub screen: bool,
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
    /// 一次性接入凭证（跨设备推流：出示对方设备签发的 ShareToken，直接推入
    /// 对方受控中继；格式为 ShareToken JSON 字符串，可用 `ctrl share-token` 生成）
    #[arg(long)]
    pub share_token: Option<String>,
    /// 把会话起点墙上时刻（Unix 毫秒）写入该文件（延迟校准用；接收端
    /// `receive --calibrate` 读同一文件，双 PC 同机同钟时得到绝对端到端延迟）
    #[arg(long)]
    pub report_start: Option<String>,
    /// 画质
    #[arg(long, value_enum, default_value_t = QualityArg::Medium)]
    pub quality: QualityArg,
}

pub async fn run(args: PushArgs) -> anyhow::Result<()> {
    let stream_id = StreamId::from(
        args.stream_id
            .unwrap_or_else(|| format!("demo-{}", std::process::id())),
    );
    let mut cfg = if args.screen {
        let mut c = StreamConfig {
            stream_id: stream_id.clone(),
            title: "CLI 屏幕推流".into(),
            video: Some(VideoSource::Screen),
            quality: args.quality.quality(),
            audio: None,
            duration_secs: Some(args.secs as u32),
            share_token: args.share_token.clone(),
        };
        if args.audio {
            // 真实屏幕共享 + 麦克风：用系统默认输入（非合成 sine）
            c.audio = Some(AudioSourceConfig::default());
        }
        c
    } else {
        StreamConfig::cli_synthetic(
            stream_id.clone(),
            "CLI 推流".into(),
            args.quality.quality(),
            args.secs as u32,
            args.audio,
            args.share_token.clone(),
        )
    };

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
            SenderEngine::start(
                cfg,
                Arc::new(FfmpegBackend::new()),
                args.relay.clone(),
                args.port,
            )
            .await?
        }
        Err(e) => return Err(e),
    };

    if let Some(port) = engine.relay_port() {
        tracing::info!(
            "📡 推流中（{} 秒）: {stream_id} @ 内嵌中继 ws://<本机IP>:{port}（自动选传输）",
            args.secs
        );
    } else {
        let url = args.relay.as_deref().unwrap_or("<自动>");
        tracing::info!("📡 推流中（{} 秒）: {stream_id} → {url}", args.secs);
    }
    // 延迟校准：等 ffmpeg 输出首帧（排除预热）后记录首帧墙时刻
    if let Some(path) = &args.report_start {
        let mut start_ms = None;
        for _ in 0..100 {
            if let Some(ms) = engine.first_frame_wall_unix_ms() {
                start_ms = Some(ms);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let start_ms = start_ms.or_else(|| engine.wall_start_unix_ms());
        if let Some(start_ms) = start_ms {
            let pts0 = engine.first_frame_pts_ms().unwrap_or(0);
            std::fs::write(
                path,
                format!(r#"{{"sessionStartUnixMs":{start_ms},"firstPtsMs":{pts0}}}"#),
            )
            .map_err(|e| anyhow::anyhow!("写校准文件失败 {path}: {e}"))?;
            tracing::info!("会话起点已写 {path}: sessionStartUnixMs={start_ms} firstPtsMs={pts0}");
        }
    }
    tokio::time::sleep(Duration::from_secs(args.secs)).await;
    engine.stop().await;
    tracing::info!("推流结束: {stream_id}");
    Ok(())
}
