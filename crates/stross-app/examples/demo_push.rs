//! 无头演示：不依赖屏幕/摄像头，用 ffmpeg 测试画面推流。
//!
//! 用法:
//! ```text
//! cargo run -p stross-app --example demo_push -- 10               # 纯视频
//! cargo run -p stross-app --example demo_push -- 10 --audio       # 加正弦波音频
//! cargo run -p stross-app --example demo_push -- 10 --port 18777  # 内嵌中继固定端口
//! cargo run -p stross-app --example demo_push -- 10 --stream-id demo  # 固定流 id
//! # 接收端用原生播放：GUI「接收」页 / demo_receive / stross receive（D1 无浏览器观看端）
//! # 或配合 demo_receive 做本地双实例串流测试（见 /home/vesita/AI/run_stream_test.sh）
//! ```

use std::sync::Arc;
use std::time::Duration;

use stross_app::SenderEngine;
use stross_media::capture::FfmpegBackend;
use stross_media::pipeline::{AudioSourceConfig, Quality, StreamConfig, VideoSource};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let mut secs: u64 = 15;
    let mut with_audio = false;
    let mut relay_port: u16 = 0; // 0 = 随机端口
    let mut stream_id = format!("demo-{}", std::process::id());
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--audio" => with_audio = true,
            "--port" => relay_port = args.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            "--stream-id" => stream_id = args.next().unwrap_or_default(),
            _ => {
                if let Ok(n) = a.parse() {
                    secs = n;
                }
            }
        }
    }

    let mut cfg = StreamConfig {
        stream_id: stream_id.clone(),
        title: "演示串流（测试画面）".into(),
        video: Some(VideoSource::Synthetic {
            pattern: "testsrc2".into(),
        }),
        quality: Quality::MEDIUM,
        audio: None,
        duration_secs: Some(secs as u32),
    };
    if with_audio {
        cfg.audio = Some(AudioSourceConfig {
            mic: None,
            system_audio: None,
            // lavfi sine（无设备环境可跑，验证解码+播放链路；真实麦克风走手机推流）
            synthetic: Some(440),
            ..Default::default()
        });
    }

    let engine = match SenderEngine::start(
        cfg.clone(),
        Arc::new(FfmpegBackend::new()),
        None,
        relay_port,
    )
    .await
    {
        Ok(e) => e,
        Err(_) if with_audio => {
            tracing::warn!("音频启动失败，退回纯视频");
            cfg.audio = None;
            SenderEngine::start(cfg, Arc::new(FfmpegBackend::new()), None, relay_port).await?
        }
        Err(e) => return Err(e),
    };
    let port = engine.relay_port().unwrap();
    let ips = stross_core::net::local_ips();
    tracing::info!("📡 演示推流中（{} 秒）…", secs);
    for ip in ips {
        tracing::info!("中继入口: http://{ip}:{port}/");
    }

    tokio::time::sleep(Duration::from_secs(secs)).await;
    engine.stop().await;
    tracing::info!("演示结束");
    Ok(())
}
