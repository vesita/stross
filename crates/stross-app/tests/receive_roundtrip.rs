//! 接收播放集成测试（1e）：推流 → 中继 → 原生接收解码（真实 ffmpeg）。
//!
//! 覆盖 GUI「📥 接收」页使用的同一套 API：
//! `StrossApp::start_receive` → WS watch → SessionDataManager →
//! `FfmpegPlaybackSink` 解码 → `receive_status` 统计。

use std::sync::Arc;
use std::time::{Duration, Instant};

use stross_app::{Platform, StrossApp};
use stross_media::capture::FfmpegBackend;
use stross_media::pipeline::ffmpeg_available;
use stross_media::pipeline::{Quality, StreamConfig, VideoSource};

fn cfg(stream_id: &str, secs: u32) -> StreamConfig {
    StreamConfig {
        stream_id: stream_id.into(),
        title: "接收测试".into(),
        video: Some(VideoSource::Synthetic {
            pattern: "testsrc2".into(),
        }),
        quality: Quality::LOW,
        audio: None,
        duration_secs: Some(secs),
    }
}

#[tokio::test]
async fn receive_decodes_live_stream() {
    if !ffmpeg_available() {
        eprintln!("跳过：未找到 ffmpeg");
        return;
    }
    let app = Arc::new(StrossApp::new(Platform::Desktop));
    app.set_backend(Arc::new(FfmpegBackend::new()));
    let relay = app.start_relay_on(0).await.expect("启动中继");
    let relay_ws = format!("ws://127.0.0.1:{}", relay.port);

    // 推流 3 秒（内核签发 session/stream id，D4）
    let started = app
        .start_stream(cfg("recv-test", 3), None)
        .await
        .expect("推流启动");
    assert!(!started.stream_id.is_empty(), "应返回内核签发的 stream id");

    // 开始接收：解码帧通道可取
    let recv = app
        .start_receive(relay_ws, started.stream_id.clone())
        .await
        .expect("接收启动");
    let mut frames = recv.take_frames().expect("应有帧通道");
    let frame_task = tokio::spawn(async move {
        let mut n = 0u32;
        while let Some(f) = frames.recv().await {
            assert_eq!(f.rgba.len() as u32, f.width * f.height * 4, "RGBA 帧尺寸");
            n += 1;
        }
        n
    });

    // 等待解码产出（真实 ffmpeg 编解码，给足时间）
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        let s = recv.stats();
        if s.decoded_video > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    app.stop_receive();

    let s = recv.stats();
    assert!(s.received > 0, "应收到协议帧: {s:?}");
    assert!(s.decoded_video > 0, "应解码出视频帧: {s:?}");
    let drawn = tokio::time::timeout(Duration::from_secs(3), frame_task)
        .await
        .map(|r| r.unwrap_or(0))
        .unwrap_or(0);
    assert!(drawn > 0, "解码帧通道应有帧流出");
    app.stop_stream().await.ok();
}
