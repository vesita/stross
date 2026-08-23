//! 无头演示：不依赖屏幕/摄像头，用 ffmpeg 测试画面推流。
//!
//! 用法:
//! ```text
//! cargo run -p stross-core --example demo_push -- 10          # 纯视频
//! cargo run -p stross-core --example demo_push -- 10 --audio  # 加正弦波音频
//! # 然后局域网内打开 http://<本机IP>:8777/ 观看
//! ```

use std::time::Duration;

use stross_core::pipeline::{AudioSourceConfig, Quality, StreamConfig, VideoSource};
use stross_core::sender::SenderEngine;

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
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--audio" => with_audio = true,
            _ => {
                if let Ok(n) = a.parse() {
                    secs = n;
                }
            }
        }
    }

    let mut cfg = StreamConfig {
        stream_id: format!("demo-{}", std::process::id()),
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
            ..Default::default()
        });
    }

    let engine = match SenderEngine::start(cfg.clone(), None, 0).await {
        Ok(e) => e,
        Err(_) if with_audio => {
            eprintln!("（音频启动失败，退回纯视频）");
            cfg.audio = None;
            SenderEngine::start(cfg, None, 0).await?
        }
        Err(e) => return Err(e),
    };
    let port = engine.relay_port().unwrap();
    let ips = stross_core::net::local_ips();
    println!("\n  📡 演示推流中（{} 秒）…", secs);
    for ip in ips {
        println!("     观看地址: http://{ip}:{port}/");
    }
    println!();

    tokio::time::sleep(Duration::from_secs(secs)).await;
    engine.stop().await;
    println!("演示结束");
    Ok(())
}
