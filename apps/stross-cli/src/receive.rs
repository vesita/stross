//! `stross receive`：接收并原生解码播放（D6 桌面 PlaybackSink）。
//!
//! 链路：`/ws/watch?stream=`（WS 全序不丢）→ [`SessionDataManager`] 无损通道
//! （1b）→ [`FfmpegPlaybackSink`] 解码（1c）→ 可选 RGBA 帧落盘 / 扬声器输出。
//!
//! ```text
//! stross receive --relay ws://127.0.0.1:18777 --stream demo --out /tmp/out --secs 4
//! # 产物: /tmp/out/frame_%04d.rgba + meta.txt
//! # 验证: ffmpeg -framerate 30 -f image2 -c:v rawvideo -pix_fmt rgba -s 1280x720 \
//! #        -i /tmp/out/frame_%04d.rgba -c:v libx264 /tmp/out/out.mp4
//! ```

use std::time::{Duration, Instant};

use anyhow::Context;
use clap::Args;
use futures_util::StreamExt;
use stross_core::session_channel::{ChannelKind, SessionDataManager};
use stross_media::playback::{
    AudioOut, AudioOutSpec, FfmpegPlaybackSink, PlaybackConfig, PlaybackSink, VideoOut,
};
use stross_proto::frame::Frame;
use stross_proto::message::ControlMessage;
use tokio_tungstenite::connect_async;

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
    let url = format!("{}/ws/watch?stream={}", args.relay, args.stream);
    tracing::info!("连接中继 {url}");
    let (mut ws, _) = connect_async(&url).await.context("连接中继失败")?;

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

    // 无损通道（WS 全序不丢）：收帧 → 通道缓存 → 轮询产出 → 播放
    let mut mgr = SessionDataManager::default();
    let deadline = Instant::now() + Duration::from_secs(args.secs);
    let mut received = 0u64;
    while Instant::now() < deadline {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            msg = ws.next() => match msg {
                Some(Ok(m)) => {
                    if m.is_text() {
                        let text = m.into_text()?;
                        let ctrl = ControlMessage::from_text(&text)?;
                        match ctrl {
                            ControlMessage::Ready { stream_id: _ } => {
                                tracing::info!("中继就绪，开始接收 {}", args.stream);
                            }
                            other => tracing::debug!("控制消息: {other:?}"),
                        }
                    } else if m.is_binary() {
                        let data = m.into_data();
                        if let Ok(frame) = Frame::from_bytes(&data) {
                            received += 1;
                            mgr.channel(&args.stream, ChannelKind::Lossless)
                                .push(frame, Instant::now());
                        }
                    }
                }
                Some(Err(e)) => {
                    tracing::warn!("WS 错误: {e}");
                    break;
                }
                None => break,
            },
        }
        let channel = mgr.channel(&args.stream, ChannelKind::Lossless);
        for f in channel.poll(Instant::now()) {
            let _ = session.push(f);
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
