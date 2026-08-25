//! `stross receive`：接收并原生解码播放（D6 桌面 PlaybackSink）。
//!
//! 链路：`watch`（WS / SRT / QUIC，按 `--relay` scheme 选传输，见
//! [`stross_core::watch::connect_watch`]）→ [`SessionDataManager`] 无损通道
//! （1b）→ [`FfmpegPlaybackSink`] 解码（1c）→ 可选 RGBA 帧落盘 / 扬声器输出。
//!
//! ```text
//! stross receive --relay ws://127.0.0.1:18777 --stream demo --out /tmp/out --secs 4
//! stross receive --relay srt://127.0.0.1:18778 --stream demo --out /tmp/out --secs 4
//! # 产物: /tmp/out/frame_%04d.rgba + meta.txt
//! # 验证: ffmpeg -framerate 30 -f image2 -c:v rawvideo -pix_fmt rgba -s 1280x720 \
//! #        -i /tmp/out/frame_%04d.rgba -c:v libx264 /tmp/out/out.mp4
//! ```

use std::time::{Duration, Instant};

use anyhow::Context;
use clap::Args;
use stross_core::session_channel::{ChannelKind, SessionDataManager};
use stross_core::watch;
use stross_core::SessionPacket;
use stross_media::playback::{
    AudioOut, AudioOutSpec, FfmpegPlaybackSink, PlaybackConfig, PlaybackSink, VideoOut,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum AudioOutArg {
    /// 扬声器 / 录音设备（cpal）
    Device,
    /// 解码但丢弃（无声卡环境 / 测试）
    Discard,
}

#[derive(Args, Debug)]
pub struct ReceiveArgs {
    /// 中继地址（ws://host:port）
    #[arg(long)]
    pub relay: String,
    /// 流 id
    #[arg(long)]
    pub stream: String,
    /// 输出目录（RGBA 帧落盘 + meta.txt）
    #[arg(long, default_value = "/tmp/stross-recv")]
    pub out: String,
    /// 接收时长（秒）
    #[arg(long, default_value_t = 4)]
    pub secs: u64,
    /// 音频输出方式
    #[arg(long, value_enum, default_value_t = AudioOutArg::Discard)]
    pub audio_out: AudioOutArg,
}

pub async fn run(args: ReceiveArgs) -> anyhow::Result<()> {
    std::fs::create_dir_all(&args.out)?;
    tracing::info!("连接中继 {}（流 {}）", args.relay, args.stream);
    let data = watch::connect_watch(&args.relay, &args.stream)
        .await
        .map_err(anyhow::Error::msg)
        .context("连接中继失败")?;

    // 播放会话：视频 → RGBA 通道；音频 → 设备或丢弃
    let sink = FfmpegPlaybackSink;
    let session = sink.open(PlaybackConfig {
        video: Some(VideoOut { display: None }),
        audio: Some(AudioOutSpec {
            channels: 2,
            sample_rate: 48_000,
            out: match args.audio_out {
                AudioOutArg::Device => AudioOut::Device,
                AudioOutArg::Discard => AudioOut::Discard,
            },
        }),
    })?;
    let mut frames_rx = session.take_video_frames().expect("已配置视频轨");

    // 解码帧落盘任务
    let out_dir = args.out.clone();
    let writer = tokio::spawn(async move {
        let mut n = 0u32;
        let mut size = (0u32, 0u32);
        while let Some(f) = frames_rx.recv().await {
            let name = format!("{out_dir}/frame_{n:04}.rgba");
            if let Err(e) = std::fs::write(&name, &f.rgba) {
                tracing::error!("写帧失败 {name}: {e}");
                break;
            }
            size = (f.width, f.height);
            n += 1;
        }
        (n, size)
    });

    // 无损通道（全序不丢）：收帧 → 通道缓存 → 即时产出 → 播放（消息驱动）
    let mut mgr = SessionDataManager::default();
    let start = Instant::now();
    let mut received = 0u64;
    loop {
        let remaining = Duration::from_secs(args.secs)
            .saturating_sub(start.elapsed());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, data.recv()).await {
            Ok(Ok(Some(pkt))) => match pkt {
                SessionPacket::Media(frame) => {
                    received += 1;
                    mgr.channel(&args.stream, ChannelKind::Lossless)
                        .push(frame, Instant::now());
                    let channel = mgr.channel(&args.stream, ChannelKind::Lossless);
                    for f in channel.poll(Instant::now()) {
                        let _ = session.push(f);
                    }
                }
                SessionPacket::Control(_) => {}
            },
            Ok(Ok(None)) => break,
            Ok(Err(e)) => {
                tracing::warn!("观看连接异常: {e}");
                break;
            }
            Err(_) => break, // 接收时长到
        }
    }
    // 收尾：冲净通道 + 停止播放（后台线程收尾后画面通道关闭）
    let channel = mgr.channel(&args.stream, ChannelKind::Lossless);
    for f in channel.poll(Instant::now()) {
        let _ = session.push(f);
    }
    session.stop();

    let (frames_out, size) = match tokio::time::timeout(Duration::from_secs(3), writer).await {
        Ok(Ok(v)) => v,
        _ => (0, (0, 0)),
    };

    let s = session.stats();
    tracing::info!(
        "接收 {received} 帧 | 解码视频 {frames_out} 帧 ({}x{}) | 音频块 {}/{} | 缓冲丢弃 {}",
        size.0,
        size.1,
        s.audio_blocks_out,
        s.audio_blocks_in,
        s.dropped_push
    );
    std::fs::write(
        format!("{}/meta.txt", args.out),
        format!(
            "stream={}\nwidth={}\nheight={}\nframes={frames_out}\nreceived={received}\n",
            args.stream, size.0, size.1
        ),
    )?;
    Ok(())
}
