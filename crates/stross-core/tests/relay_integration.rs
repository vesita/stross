//! 中继集成测试：推流端 → 中继 → 观看端全链路。
//!
//! 测试不依赖 ffmpeg，直接构造协议帧推入，验证：
//! * REST `/api/streams` 列出流
//! * 观看端先收到 `Ready`，且视频只在关键帧后转发
//! * 推流端断开后流被移除

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use stross_core::relay::RelayServer;
use stross_proto::frame::{Frame, FLAG_KEYFRAME, TRACK_AUDIO, TRACK_VIDEO};
use stross_proto::message::{ControlMessage, TrackInfo};
use tokio_tungstenite::tungstenite::Message;

fn video_frame(keyframe: bool) -> Vec<u8> {
    // 伪装一个 H.264 访问单元（无需真实可解码）
    let payload = if keyframe {
        vec![0x67, 0x00, 0x01, 0x02, 0x05, 0x03]
    } else {
        vec![0x41, 0x00, 0x01, 0x02]
    };
    Frame::new(
        TRACK_VIDEO,
        stross_proto::frame::CODEC_H264,
        if keyframe { FLAG_KEYFRAME } else { 0 },
        0,
        payload,
    )
    .to_bytes()
    .to_vec()
}

fn audio_frame() -> Vec<u8> {
    Frame::new(
        TRACK_AUDIO,
        stross_proto::frame::CODEC_AAC,
        0,
        0,
        vec![0xFF, 0xF1, 0x50, 0x00, 0x01, 0x1F, 0xFC, 0x00],
    )
    .to_bytes()
    .to_vec()
}

fn hello() -> String {
    ControlMessage::Hello {
        stream_id: "test-stream".into(),
        title: "测试串流".into(),
        video: Some(TrackInfo {
            codec: "h264".into(),
            width: Some(640),
            height: Some(360),
            fps: Some(30),
            sample_rate: None,
            channels: None,
        }),
        audio: Some(TrackInfo {
            codec: "aac".into(),
            width: None,
            height: None,
            fps: None,
            sample_rate: Some(48000),
            channels: Some(2),
        }),
    }
    .to_text()
}

#[tokio::test]
async fn push_watch_relay_roundtrip() {
    let handle = RelayServer::start(0).await.unwrap();
    let port = handle.port;
    let base = format!("ws://127.0.0.1:{port}");

    // ---- 推流端 ----
    let (mut push, _) = tokio_tungstenite::connect_async(format!("{base}/ws/push"))
        .await
        .expect("连接推流端点");
    push.send(Message::Text(hello().into())).await.unwrap();
    let welcome = push.next().await.unwrap().unwrap();
    assert!(welcome.is_text(), "应收到 Welcome");
    let welcome = ControlMessage::from_text(&welcome.into_text().unwrap()).unwrap();
    assert_eq!(welcome, ControlMessage::Welcome { stream_id: "test-stream".into() });

    // 先推一个非关键帧（观看端应忽略），再推关键帧
    push.send(Message::Binary(video_frame(false).into())).await.unwrap();
    push.send(Message::Binary(video_frame(true).into())).await.unwrap();
    push.send(Message::Binary(audio_frame().into())).await.unwrap();

    // ---- REST 流列表 ----
    let body = reqwest_lite(&format!("http://127.0.0.1:{port}/api/streams")).await;
    let streams: Vec<stross_proto::message::StreamInfo> = serde_json::from_str(&body).unwrap();
    assert_eq!(streams.len(), 1);
    assert_eq!(streams[0].stream_id, "test-stream");
    assert_eq!(streams[0].title, "测试串流");
    assert!(streams[0].video.is_some());

    // ---- 观看端（在关键帧之后接入，验证对齐）----
    let (mut watch, _) = tokio_tungstenite::connect_async(format!("{base}/ws/watch?stream=test-stream"))
        .await
        .expect("连接观看端点");

    // 首个消息必须是 Ready
    let first = watch.next().await.unwrap().unwrap();
    let ready = ControlMessage::from_text(&first.into_text().unwrap()).unwrap();
    assert_eq!(ready, ControlMessage::Ready { stream_id: "test-stream".into() });

    // 关键帧 + 音频帧
    let mut seen_video_keyframe = false;
    let mut seen_audio = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline && !(seen_video_keyframe && seen_audio) {
        match tokio::time::timeout(Duration::from_millis(500), watch.next()).await {
            Ok(Some(Ok(Message::Binary(bytes)))) => {
                let frame = Frame::from_bytes(&bytes).unwrap();
                if frame.header.track == TRACK_VIDEO {
                    assert!(frame.header.is_keyframe(), "观看端不应收到非关键帧");
                    seen_video_keyframe = true;
                }
                if frame.header.track == TRACK_AUDIO {
                    seen_audio = true;
                }
            }
            Ok(Some(Ok(_))) => {}
            _ => break,
        }
    }
    assert!(seen_video_keyframe, "观看端应收到视频关键帧");
    assert!(seen_audio, "观看端应收到音频帧");

    // ---- 推流端断开，流被移除 ----
    push.close(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let body = reqwest_lite(&format!("http://127.0.0.1:{port}/api/streams")).await;
    let streams: Vec<stross_proto::message::StreamInfo> = serde_json::from_str(&body).unwrap();
    assert!(streams.is_empty(), "推流端断开后流应被移除");

    handle.stop().await;
}

#[tokio::test]
async fn healthz_and_index_served() {
    let handle = RelayServer::start(0).await.unwrap();
    let port = handle.port;
    let health = reqwest_lite(&format!("http://127.0.0.1:{port}/healthz")).await;
    assert_eq!(health, "ok");
    let index = reqwest_lite(&format!("http://127.0.0.1:{port}/")).await;
    assert!(index.contains("Stross"));
    let js = reqwest_lite(&format!("http://127.0.0.1:{port}/app.js")).await;
    assert!(js.contains("JMuxer"));
    handle.stop().await;
}

#[tokio::test]
async fn watch_unknown_stream_gets_error() {
    let handle = RelayServer::start(0).await.unwrap();
    let (mut watch, _) = tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{}/ws/watch?stream=nope",
        handle.port
    ))
    .await
    .unwrap();
    let msg = watch.next().await.unwrap().unwrap();
    let ctrl = ControlMessage::from_text(&msg.into_text().unwrap()).unwrap();
    assert!(matches!(ctrl, ControlMessage::Error { .. }));
    handle.stop().await;
}

/// 极简 HTTP GET（避免引入 reqwest 依赖）。
async fn reqwest_lite(url: &str) -> String {
    let resp = reqwest::get(url).await.expect("HTTP 请求失败");
    resp.text().await.unwrap()
}
