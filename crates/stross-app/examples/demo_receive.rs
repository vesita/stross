//! 无头接收端演示：从局域网中继**接收并原生解码**（D6 桌面 PlaybackSink）。
//!
//! 本地双实例串流测试（实例 A = `demo_push` 内嵌中继 + 推流，实例 B = 本示例）：
//!
//! ```text
//! cargo run -p stross-app --example demo_receive -- ws://127.0.0.1:18777 demo /tmp/out 4
//! # 参数: <中继 ws 地址> <流 id> <输出目录> [接收秒数]
//! # 产物: <输出目录>/frame_%04d.rgba（RGBA8888 裸帧）+ meta.txt（宽高/帧数）
//! # 验证: ffmpeg -framerate 30 -f rawvideo -pix_fmt rgba -s 1280x720 \
//! #        -i <输出目录>/frame_%04d.rgba <输出目录>/out.mp4
//! ```
//!
//! 链路：`/ws/watch?stream=`（WS，全序不丢）→ [`SessionDataManager`]
//! 无损通道（1b）→ [`FfmpegPlaybackSink`] 解码（1c）→ RGBA 帧落盘；
//! 音频轨解码但丢弃（`AudioOut::Discard`，统计块数证明解码链路）。

use std::time::{Duration, Instant};

use anyhow::Context;
use futures_util::StreamExt;
use stross_core::session_channel::{ChannelKind, SessionDataManager};
use stross_media::playback::{
    AudioOut, AudioOutSpec, FfmpegPlaybackSink, PlaybackConfig, PlaybackSink, VideoOut,
};
use stross_proto::frame::Frame;
use stross_proto::message::ControlMessage;
use tokio_tungstenite::connect_async;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let relay = args.next().unwrap_or_else(|| "ws://127.0.0.1:18777".into());
    let stream_id = args.next().unwrap_or_else(|| "demo".into());
    let out_dir = args.next().unwrap_or_else(|| "/tmp/stross-recv".into());
    let seconds: u64 = args.next().and_then(|v| v.parse().ok()).unwrap_or(4);

    std::fs::create_dir_all(&out_dir)?;
    let url = format!("{relay}/ws/watch?stream={stream_id}");
    tracing::info!("连接中继 {url}");
    let (mut ws, _) = connect_async(&url).await.context("连接中继失败")?;

    // 播放会话：视频 → RGBA 通道；音频 → 解码丢弃（无声卡环境可跑）
    let sink = FfmpegPlaybackSink;
    let session = sink.open(PlaybackConfig {
        video: Some(VideoOut { display: None }),
        audio: Some(AudioOutSpec {
            channels: 2,
            sample_rate: 48_000,
            out: AudioOut::Discard,
        }),
    })?;
    let mut frames_rx = session.take_video_frames().expect("已配置视频轨");

    // 解码帧落盘任务（输出目录下 frame_%04d.rgba）
    let out_dir2 = out_dir.clone();
    let writer = tokio::spawn(async move {
        let mut n = 0u32;
        let mut size = (0u32, 0u32);
        while let Some(f) = frames_rx.recv().await {
            let name = format!("{out_dir2}/frame_{n:04}.rgba");
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
    let deadline = Instant::now() + Duration::from_secs(seconds);
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
                                tracing::info!("中继就绪，开始接收 {stream_id}");
                            }
                            other => tracing::debug!("控制消息: {other:?}"),
                        }
                    } else if m.is_binary() {
                        let data = m.into_data();
                        if let Ok(frame) = Frame::from_bytes(&data) {
                            received += 1;
                            mgr.channel(&stream_id, ChannelKind::Lossless)
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
        // 通道 → 播放（lossless 直通，按序产出）
        let channel = mgr.channel(&stream_id, ChannelKind::Lossless);
        for f in channel.poll(Instant::now()) {
            let _ = session.push(f);
        }
    }
    // 收尾：冲净通道 + 停止播放（后台线程收尾后画面通道关闭）
    let channel = mgr.channel(&stream_id, ChannelKind::Lossless);
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
        format!("{out_dir}/meta.txt"),
        format!(
            "stream={stream_id}\nwidth={}\nheight={}\nframes={frames_out}\nreceived={received}\n",
            size.0, size.1
        ),
    )?;
    Ok(())
}
